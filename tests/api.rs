//! Pins properties of the public API that could otherwise change silently.

use core::mem::size_of;

use rfc2217::{
    Client, ComPortState, CommandError, DataBits, Event, FlowControl, InlineBytes, LineState,
    ModemState, NotReady, Parity, PartialSettings, PortSettings, Progress, Purge, Server,
    ServerEvent, Setting, SettingSet, StopBits,
};

/// Compiles only if `T` is `Send + Sync + Unpin + Debug`.
fn assert_traits<T: Send + Sync + Unpin + core::fmt::Debug>() {}

/// Compiles only if `T` is an error type.
fn assert_error<T: core::error::Error>() {}

/// Compiles only if `T` is a plain value: `Copy`, comparable and hashable.
fn assert_value<T: Copy + Eq + core::hash::Hash>() {}

/// Compiles only if `T` has a `Default`.
fn assert_default<T: Default>() {}

/// Compiles only if `T` has a `Display`.
fn assert_display<T: core::fmt::Display>() {}

#[test]
fn public_types_are_send_sync_unpin_debug() {
    assert_traits::<Client>();
    assert_traits::<Server>();
    assert_traits::<Event>();
    assert_traits::<ServerEvent>();
    assert_traits::<ComPortState>();
    assert_traits::<PortSettings>();
    assert_traits::<DataBits>();
    assert_traits::<Parity>();
    assert_traits::<StopBits>();
    assert_traits::<FlowControl>();
    assert_traits::<Purge>();
    assert_traits::<PartialSettings>();
    assert_traits::<Setting>();
    assert_traits::<SettingSet>();
    assert_traits::<ModemState>();
    assert_traits::<LineState>();
    assert_traits::<InlineBytes>();
    assert_traits::<Progress>();
    assert_traits::<CommandError>();
    assert_traits::<NotReady>();
    assert_error::<CommandError>();
    assert_error::<NotReady>();
}

#[test]
fn value_types_are_copy_eq_hash() {
    assert_value::<ComPortState>();
    assert_value::<PortSettings>();
    assert_value::<DataBits>();
    assert_value::<Parity>();
    assert_value::<StopBits>();
    assert_value::<FlowControl>();
    assert_value::<Purge>();
    assert_value::<PartialSettings>();
    assert_value::<Setting>();
    assert_value::<SettingSet>();
    assert_value::<ModemState>();
    assert_value::<LineState>();
    assert_value::<InlineBytes>();
    assert_value::<Progress>();
    assert_value::<CommandError>();
    assert_value::<NotReady>();

    assert_default::<PortSettings>();
    assert_default::<PartialSettings>();
    assert_default::<SettingSet>();
    assert_default::<ModemState>();
    assert_default::<LineState>();
    assert_default::<InlineBytes>();

    assert_display::<Setting>();
    assert_display::<SettingSet>();
    assert_display::<CommandError>();
    assert_display::<NotReady>();
}

/// The closed RFC 2217 records and value sets are exhaustive (Q9): matches
/// without a wildcard arm and struct literals compile outside the crate.
#[test]
fn closed_types_are_exhaustive() {
    fn state(s: ComPortState) -> u8 {
        match s {
            ComPortState::Negotiating => 0,
            ComPortState::Active => 1,
            ComPortState::Refused => 2,
        }
    }
    fn setting(s: Setting) -> u8 {
        match s {
            Setting::BaudRate => 0,
            Setting::DataBits => 1,
            Setting::Parity => 2,
            Setting::StopBits => 3,
            Setting::FlowControl => 4,
        }
    }
    fn data_bits(d: DataBits) -> u8 {
        match d {
            DataBits::Five => 5,
            DataBits::Six => 6,
            DataBits::Seven => 7,
            DataBits::Eight => 8,
        }
    }
    fn parity(p: Parity) -> u8 {
        match p {
            Parity::None => 1,
            Parity::Odd => 2,
            Parity::Even => 3,
            Parity::Mark => 4,
            Parity::Space => 5,
        }
    }
    fn stop_bits(s: StopBits) -> u8 {
        match s {
            StopBits::One => 1,
            StopBits::Two => 2,
            StopBits::OnePointFive => 3,
        }
    }
    fn flow_control(f: FlowControl) -> u8 {
        match f {
            FlowControl::None => 1,
            FlowControl::XonXoff => 2,
            FlowControl::RtsCts => 3,
            FlowControl::Dcd => 17,
            FlowControl::Dsr => 19,
        }
    }
    fn purge(p: Purge) -> u8 {
        match p {
            Purge::Receive => 1,
            Purge::Transmit => 2,
            Purge::Both => 3,
        }
    }

    let settings = PortSettings {
        baud_rate: 9600,
        data_bits: DataBits::Eight,
        parity: Parity::None,
        stop_bits: StopBits::One,
        flow_control: FlowControl::None,
    };
    assert_eq!(settings, PortSettings::default());
    let PortSettings {
        baud_rate,
        data_bits: d,
        parity: p,
        stop_bits: s,
        flow_control: f,
    } = settings;
    let reported = PartialSettings {
        baud_rate: Some(baud_rate),
        data_bits: Some(d),
        parity: Some(p),
        stop_bits: Some(s),
        flow_control: Some(f),
    };
    assert_eq!(reported.complete(), Some(settings));
    assert_eq!(
        [data_bits(d), parity(p), stop_bits(s), flow_control(f)],
        [8, 1, 1, 1]
    );
    assert_eq!(state(ComPortState::Refused), 2);
    assert_eq!(setting(Setting::FlowControl), 4);
    assert_eq!(purge(Purge::Both), 3);
}

#[test]
fn sizes() {
    for (name, size) in [
        ("Client", size_of::<Client>()),
        ("Server", size_of::<Server>()),
        ("Event", size_of::<Event>()),
        ("ServerEvent", size_of::<ServerEvent>()),
        ("Option<Event>", size_of::<Option<Event>>()),
        ("InlineBytes", size_of::<InlineBytes>()),
        ("Progress", size_of::<Progress>()),
        ("CommandError", size_of::<CommandError>()),
    ] {
        println!("size_of::<{name}>() = {size}");
    }
    assert_eq!(size_of::<Event>(), 68);
    assert_eq!(size_of::<ServerEvent>(), 68);
    assert_eq!(size_of::<Option<Event>>(), size_of::<Event>());
    assert_eq!(size_of::<InlineBytes>(), 66);
    assert_eq!(size_of::<CommandError>(), 1);
    if cfg!(target_pointer_width = "64") {
        assert!(size_of::<Client>() <= 560);
        assert!(size_of::<Server>() <= 536);
    }
}
