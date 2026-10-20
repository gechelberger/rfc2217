//! The line settings in the pty's termios: the initial values from the
//! command line, the changes a local program makes, and the bridge's rate
//! written back.
//!
//! Changes are found by polling (about every [`SettingsWatch::INTERVAL`]),
//! not with packet mode: packet mode reports settings changes only with
//! EXTPROC set, and EXTPROC turns off the slave's canonical mode, echo,
//! signals, CR/NL translation and XON/XOFF on input, which breaks
//! line-oriented programs. A poll sees only the latest state: settings that a
//! program sets and changes again within one interval are never seen, and a
//! change takes effect when seen, not at its position in the data.

use std::io;
use std::time::Duration;

use rfc2217::{DataBits, FlowControl, Parity, PartialSettings, PortSettings, StopBits};
use rustix::termios::{ControlModes, InputModes, Termios};

use super::sys::Pty;

/// Whether the data bits and parity that a local program sets are read from
/// the pty and reported as changes. Linux: never. The pty driver clears
/// CSIZE and PARENB and sets CS8 on every termios change (pty_set_termios in
/// drivers/tty/pty.c), so a program that asks for 7E1 gets 8N, silently or,
/// with Debian's and Ubuntu's glibc, with EINVAL.
#[cfg(not(target_os = "macos"))]
pub(crate) const FORWARD_FRAMING: bool = false;

/// Whether the data bits and parity that a local program sets are read from
/// the pty and reported as changes. macOS: XNU's ptys have no parameter
/// routine, so its source suggests CS7 and PARENB are kept, but that is
/// untested: off until `macos_keeps_framing` (an ignored test, run with
/// `cargo test -- --ignored` on a Mac) passes.
#[cfg(target_os = "macos")]
pub(crate) const FORWARD_FRAMING: bool = false;

/// What the pty's termios says, decoded into what RFC 2217 can carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PtySettings {
    /// The output speed, the one rate forwarded. 0 is B0, which hangs up a
    /// real port and is never forwarded: RFC 2217 reads a rate of 0 as a
    /// query.
    pub(crate) baud_rate: u32,
    /// The input speed, when the program set one that differs from the
    /// output speed. RFC 2217 has a single rate, so it is only reported.
    pub(crate) split_input_speed: Option<u32>,
    /// CSTOPB: two stop bits, else one. 1.5 (CS5 with CSTOPB on a UART) is
    /// seen only with [`FORWARD_FRAMING`], since a Linux pty never holds CS5.
    pub(crate) stop_bits: StopBits,
    /// CRTSCTS, else IXON or IXOFF, else none. termios has no DCD or DSR
    /// flow control.
    pub(crate) flow_control: FlowControl,
    /// `None` unless [`FORWARD_FRAMING`].
    pub(crate) data_bits: Option<DataBits>,
    /// `None` unless [`FORWARD_FRAMING`].
    pub(crate) parity: Option<Parity>,
}

impl PtySettings {
    /// Decodes `termios`, with the data bits and parity only if `framing`.
    pub(crate) fn decode(termios: &Termios, framing: bool) -> Self {
        let c = termios.control_modes;
        let data_bits = framing.then(|| match c & ControlModes::CSIZE {
            m if m == ControlModes::CS5 => DataBits::Five,
            m if m == ControlModes::CS6 => DataBits::Six,
            m if m == ControlModes::CS7 => DataBits::Seven,
            _ => DataBits::Eight,
        });
        let parity = framing.then(|| decode_parity(c));
        let stop_bits = match (c.contains(ControlModes::CSTOPB), data_bits) {
            (false, _) => StopBits::One,
            (true, Some(DataBits::Five)) => StopBits::OnePointFive,
            (true, _) => StopBits::Two,
        };
        let flow_control = if c.contains(ControlModes::CRTSCTS) {
            FlowControl::RtsCts
        } else if termios
            .input_modes
            .intersects(InputModes::IXON | InputModes::IXOFF)
        {
            FlowControl::XonXoff
        } else {
            FlowControl::None
        };
        let output = termios.output_speed();
        let input = termios.input_speed();
        Self {
            baud_rate: output,
            // An input speed of 0 means "the same as the output speed".
            split_input_speed: (input != 0 && input != output).then_some(input),
            stop_bits,
            flow_control,
            data_bits,
            parity,
        }
    }

    /// Both speeds, for comparing.
    fn speeds(&self) -> (u32, Option<u32>) {
        (self.baud_rate, self.split_input_speed)
    }

    /// The forwardable settings that differ in `self` from `before`, `None`
    /// for each one that did not change. A change to B0 is left out.
    fn changes_since(&self, before: &Self) -> PartialSettings {
        fn changed<T: PartialEq + Copy>(now: T, before: T) -> Option<T> {
            (now != before).then_some(now)
        }
        PartialSettings {
            baud_rate: changed(self.baud_rate, before.baud_rate).filter(|&rate| rate != 0),
            data_bits: changed(self.data_bits, before.data_bits).flatten(),
            parity: changed(self.parity, before.parity).flatten(),
            stop_bits: changed(self.stop_bits, before.stop_bits),
            flow_control: changed(self.flow_control, before.flow_control),
        }
    }
}

fn decode_parity(c: ControlModes) -> Parity {
    let odd = c.contains(ControlModes::PARODD);
    if !c.contains(ControlModes::PARENB) {
        return Parity::None;
    }
    // Mark and space parity (CMSPAR) exist only on Linux.
    #[cfg(target_os = "linux")]
    if c.contains(ControlModes::CMSPAR) {
        return if odd { Parity::Mark } else { Parity::Space };
    }
    if odd { Parity::Odd } else { Parity::Even }
}

/// Puts the initial termios in raw mode with the speed, stop bits and flow
/// control of `settings` (see [`Pty::open`]).
pub(super) fn seed(termios: &mut Termios, settings: &PortSettings) -> rustix::io::Result<()> {
    // Raw mode: no echo back to the bridge, no output processing, and IXON
    // off. The kernel's default pty termios has IXON on, which would take
    // 0x11 and 0x13 out of the data and report XON/XOFF flow control for
    // every program that leaves it alone.
    termios.make_raw();
    termios.input_modes -= InputModes::IXON | InputModes::IXOFF | InputModes::IXANY;
    termios.control_modes -= ControlModes::CSTOPB | ControlModes::CRTSCTS;
    if settings.stop_bits == StopBits::Two {
        termios.control_modes |= ControlModes::CSTOPB;
    }
    match settings.flow_control {
        FlowControl::RtsCts => termios.control_modes |= ControlModes::CRTSCTS,
        // As a program asking for XON/XOFF (pyserial's xonxoff) sets it.
        FlowControl::XonXoff => termios.input_modes |= InputModes::IXON | InputModes::IXOFF,
        FlowControl::None | FlowControl::Dcd | FlowControl::Dsr => {}
    }
    match settings.baud_rate {
        0 => Ok(()),
        rate => termios.set_speed(rate),
    }
}

/// Settings the local program changed, from [`SettingsWatch::poll`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Update {
    /// The pty's settings now, for messages: e.g. a split input speed, or a
    /// change to B0 that is not forwarded.
    pub(crate) now: PtySettings,
    /// What to forward: `None` for each setting the program did not change.
    /// All `None` when only the input speed changed, or the output speed
    /// changed to B0.
    pub(crate) changed: PartialSettings,
}

impl Update {
    /// Writes the changed settings into `settings` (the bridge's requested
    /// settings); the ones the program did not change keep their values, so
    /// a `--stop-bits 1.5` or `--flow dcd` from the command line stays until
    /// the program sets stop bits or flow control itself. Returns whether
    /// `settings` changed.
    pub(crate) fn apply(&self, settings: &mut PortSettings) -> bool {
        let before = *settings;
        let c = &self.changed;
        settings.baud_rate = c.baud_rate.unwrap_or(settings.baud_rate);
        settings.data_bits = c.data_bits.unwrap_or(settings.data_bits);
        settings.parity = c.parity.unwrap_or(settings.parity);
        settings.stop_bits = c.stop_bits.unwrap_or(settings.stop_bits);
        settings.flow_control = c.flow_control.unwrap_or(settings.flow_control);
        *settings != before
    }
}

/// Finds the settings changes a local program makes, by comparing
/// successive reads of the pty's termios.
///
/// Create it right after [`Pty::open`], before the path is shown to anyone:
/// what the pty says then is the baseline, and only later differences count
/// as the program's.
#[derive(Debug)]
pub(crate) struct SettingsWatch {
    last: PtySettings,
}

impl SettingsWatch {
    /// How often to [`poll`](Self::poll): about as fast as a person notices,
    /// and one ioctl each time.
    pub(crate) const INTERVAL: Duration = Duration::from_millis(50);

    pub(crate) fn new(pty: &Pty) -> io::Result<Self> {
        Ok(Self { last: read(pty)? })
    }

    /// The settings as last seen (or written by [`set_speed`](Self::set_speed)).
    #[cfg(all(test, target_os = "linux"))]
    pub(crate) fn current(&self) -> PtySettings {
        self.last
    }

    /// Reads the termios and returns what the program changed since the
    /// last call, or `None` when nothing did.
    pub(crate) fn poll(&mut self, pty: &Pty) -> io::Result<Option<Update>> {
        let now = read(pty)?;
        if now == self.last {
            return Ok(None);
        }
        let changed = now.changes_since(&self.last);
        self.last = now;
        Ok(Some(Update { now, changed }))
    }

    /// Writes the bridge's rate (input and output speed) into the pty, so
    /// the program sees the rate the line really runs at; this is not a
    /// change by the program, and [`poll`](Self::poll) does not report it.
    ///
    /// Returns whether the pty now shows `rate`. It does not when `rate` is 0
    /// (B0 would hang up the device), or when the program changed the speed
    /// since the last poll: its newer request wins, and the next poll
    /// reports it. The read and the write are not atomic: a program changing
    /// other settings in the microseconds between them loses that change.
    pub(crate) fn set_speed(&mut self, pty: &Pty, rate: u32) -> io::Result<bool> {
        if rate == 0 {
            return Ok(false);
        }
        let mut termios = pty.termios()?;
        let now = PtySettings::decode(&termios, FORWARD_FRAMING);
        if now.speeds() != self.last.speeds() {
            return Ok(false);
        }
        if now.speeds() != (rate, None) {
            termios
                .set_speed(rate)
                .map_err(super::sys::failed(format_args!(
                    "setting {} to {rate} baud",
                    pty.path().display()
                )))?;
            pty.set_termios(&termios)?;
        }
        // Only the speed is taken as seen: any other setting the program
        // changed since the last poll is still reported by the next one.
        self.last.baud_rate = rate;
        self.last.split_input_speed = None;
        Ok(true)
    }
}

fn read(pty: &Pty) -> io::Result<PtySettings> {
    Ok(PtySettings::decode(&pty.termios()?, FORWARD_FRAMING))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pty_settings(baud_rate: u32) -> PtySettings {
        PtySettings {
            baud_rate,
            split_input_speed: None,
            stop_bits: StopBits::One,
            flow_control: FlowControl::None,
            data_bits: None,
            parity: None,
        }
    }

    #[test]
    fn changes_are_per_setting() {
        let before = pty_settings(9600);
        let now = PtySettings {
            stop_bits: StopBits::Two,
            ..before
        };
        assert_eq!(
            now.changes_since(&before),
            PartialSettings {
                stop_bits: Some(StopBits::Two),
                ..Default::default()
            }
        );
        // B0 is not forwarded; leaving it is.
        let hangup = pty_settings(0);
        assert_eq!(hangup.changes_since(&before), PartialSettings::default());
        assert_eq!(before.changes_since(&hangup).baud_rate, Some(9600));
    }

    #[test]
    fn apply_keeps_what_the_program_did_not_change() {
        let mut requested = PortSettings {
            baud_rate: 0,
            stop_bits: StopBits::OnePointFive,
            flow_control: FlowControl::Dcd,
            ..Default::default()
        };
        let update = Update {
            now: pty_settings(57_600),
            changed: PartialSettings {
                baud_rate: Some(57_600),
                ..Default::default()
            },
        };
        assert!(update.apply(&mut requested));
        assert_eq!(
            requested,
            PortSettings {
                baud_rate: 57_600,
                stop_bits: StopBits::OnePointFive,
                flow_control: FlowControl::Dcd,
                ..Default::default()
            }
        );
        assert!(!update.apply(&mut requested));
    }
}

#[cfg(all(test, target_os = "linux"))]
mod linux_tests {
    use rustix::termios::ControlModes as C;
    use rustix::termios::InputModes as I;

    use super::*;
    use crate::pty::testing::{Program, settings};

    fn watched(baud_rate: u32) -> (Pty, SettingsWatch) {
        let pty = Pty::open(&settings(baud_rate)).unwrap();
        let watch = SettingsWatch::new(&pty).unwrap();
        (pty, watch)
    }

    #[test]
    fn forwards_what_a_program_sets_except_framing() {
        let (pty, mut watch) = watched(9600);
        assert_eq!(watch.poll(&pty).unwrap(), None);
        let program = Program::open(&pty);
        // 7E2 with RTS/CTS at 115200, through rustix's own TCSETS2, so the
        // outcome does not depend on the C library: Debian's and Ubuntu's
        // glibc fail some CS7 and PARENB requests with EINVAL, others don't.
        // So the call's result is not checked, only what the pty holds.
        let _ = program.try_change(|t| {
            t.control_modes -= C::CSIZE;
            t.control_modes |= C::CS7 | C::PARENB | C::CSTOPB | C::CRTSCTS;
            t.set_speed(115_200).unwrap();
        });
        let held = program.termios().control_modes;
        assert_eq!(held & C::CSIZE, C::CS8);
        assert!(!held.contains(C::PARENB));

        let update = watch.poll(&pty).unwrap().unwrap();
        assert_eq!(
            update.changed,
            PartialSettings {
                baud_rate: Some(115_200),
                data_bits: None,
                parity: None,
                stop_bits: Some(StopBits::Two),
                flow_control: Some(FlowControl::RtsCts),
            }
        );
        assert_eq!((update.now.data_bits, update.now.parity), (None, None));
        assert_eq!(watch.poll(&pty).unwrap(), None);
    }

    #[test]
    fn arbitrary_rates_and_split_speeds() {
        let (pty, mut watch) = watched(9600);
        let program = Program::open(&pty);
        program.change(|t| t.set_speed(250_000).unwrap());
        let update = watch.poll(&pty).unwrap().unwrap();
        assert_eq!(update.changed.baud_rate, Some(250_000));
        assert_eq!(update.now.split_input_speed, None);

        program.change(|t| {
            t.set_input_speed(9600).unwrap();
            t.set_output_speed(115_200).unwrap();
        });
        let update = watch.poll(&pty).unwrap().unwrap();
        assert_eq!(update.changed.baud_rate, Some(115_200));
        assert_eq!(update.now.split_input_speed, Some(9600));

        // Only the input speed: reported, nothing to forward.
        program.change(|t| t.set_input_speed(4800).unwrap());
        let update = watch.poll(&pty).unwrap().unwrap();
        assert_eq!(update.changed, PartialSettings::default());
        assert_eq!(update.now.split_input_speed, Some(4800));
    }

    #[test]
    fn b0_is_not_forwarded() {
        let (pty, mut watch) = watched(9600);
        let program = Program::open(&pty);
        program.change(|t| t.set_speed(0).unwrap());
        let update = watch.poll(&pty).unwrap().unwrap();
        assert_eq!(update.now.baud_rate, 0);
        assert_eq!(update.changed.baud_rate, None);
    }

    #[test]
    fn flow_control() {
        let (pty, mut watch) = watched(9600);
        let program = Program::open(&pty);
        let mut flow = |set: I, clear: I, rtscts: bool| {
            program.change(|t| {
                t.input_modes -= clear;
                t.input_modes |= set;
                t.control_modes.set(C::CRTSCTS, rtscts);
            });
            watch.poll(&pty).unwrap().map(|u| u.now.flow_control)
        };
        assert_eq!(flow(I::IXON, I::empty(), false), Some(FlowControl::XonXoff));
        assert_eq!(flow(I::empty(), I::IXON, false), Some(FlowControl::None));
        assert_eq!(
            flow(I::IXOFF, I::empty(), false),
            Some(FlowControl::XonXoff)
        );
        assert_eq!(flow(I::IXON, I::empty(), false), None);
        // CRTSCTS wins over IXON/IXOFF.
        assert_eq!(
            flow(I::empty(), I::empty(), true),
            Some(FlowControl::RtsCts)
        );
        assert_eq!(
            flow(I::empty(), I::IXON | I::IXOFF, false),
            Some(FlowControl::None)
        );
    }

    #[test]
    fn the_bridges_rate_is_not_a_program_change() {
        let (pty, mut watch) = watched(0);
        assert!(watch.set_speed(&pty, 250_000).unwrap());
        let program = Program::open(&pty);
        let t = program.termios();
        assert_eq!((t.input_speed(), t.output_speed()), (250_000, 250_000));
        assert_eq!(watch.poll(&pty).unwrap(), None);
        assert_eq!(watch.current().baud_rate, 250_000);
        // A program's change after it still counts.
        program.change(|t| t.set_speed(57_600).unwrap());
        assert_eq!(
            watch.poll(&pty).unwrap().unwrap().changed.baud_rate,
            Some(57_600)
        );
        // The same rate again changes nothing and needs no write.
        assert!(watch.set_speed(&pty, 57_600).unwrap());
        assert!(!watch.set_speed(&pty, 0).unwrap());
        assert_eq!(program.termios().output_speed(), 57_600);
    }

    #[test]
    fn set_speed_yields_to_an_unseen_program_change() {
        let (pty, mut watch) = watched(9600);
        let program = Program::open(&pty);
        program.change(|t| {
            t.set_speed(4800).unwrap();
            t.control_modes |= C::CSTOPB;
        });
        assert!(!watch.set_speed(&pty, 115_200).unwrap());
        assert_eq!(program.termios().output_speed(), 4800);
        let update = watch.poll(&pty).unwrap().unwrap();
        assert_eq!(update.changed.baud_rate, Some(4800));
        assert_eq!(update.changed.stop_bits, Some(StopBits::Two));
    }

    #[test]
    fn set_speed_keeps_other_unseen_changes() {
        let (pty, mut watch) = watched(9600);
        let program = Program::open(&pty);
        program.change(|t| t.control_modes |= C::CRTSCTS);
        assert!(watch.set_speed(&pty, 19_200).unwrap());
        let t = program.termios();
        assert!(t.control_modes.contains(C::CRTSCTS));
        assert_eq!(t.output_speed(), 19_200);
        let update = watch.poll(&pty).unwrap().unwrap();
        assert_eq!(
            update.changed,
            PartialSettings {
                flow_control: Some(FlowControl::RtsCts),
                ..Default::default()
            }
        );
    }

    #[test]
    fn decodes_framing_when_asked() {
        // The decoding itself, on a termios value that never reaches the
        // kernel (which would force 8N).
        let pty = Pty::open(&settings(9600)).unwrap();
        let mut t = pty.termios().unwrap();
        let mut framing = |modes: C| {
            t.control_modes -= C::CSIZE | C::PARENB | C::PARODD | C::CMSPAR | C::CSTOPB;
            t.control_modes |= modes;
            let s = PtySettings::decode(&t, true);
            assert_eq!(PtySettings::decode(&t, false).data_bits, None);
            (s.data_bits.unwrap(), s.parity.unwrap(), s.stop_bits)
        };
        assert_eq!(
            framing(C::CS8),
            (DataBits::Eight, Parity::None, StopBits::One)
        );
        assert_eq!(
            framing(C::CS7 | C::PARENB),
            (DataBits::Seven, Parity::Even, StopBits::One)
        );
        assert_eq!(
            framing(C::CS6 | C::PARENB | C::PARODD | C::CSTOPB),
            (DataBits::Six, Parity::Odd, StopBits::Two)
        );
        assert_eq!(
            framing(C::CS5 | C::PARENB | C::CMSPAR | C::PARODD | C::CSTOPB),
            (DataBits::Five, Parity::Mark, StopBits::OnePointFive)
        );
        assert_eq!(
            framing(C::CS8 | C::PARENB | C::CMSPAR),
            (DataBits::Eight, Parity::Space, StopBits::One)
        );
        // PARODD alone, without PARENB, is no parity.
        assert_eq!(
            framing(C::CS8 | C::PARODD),
            (DataBits::Eight, Parity::None, StopBits::One)
        );
    }
}

#[cfg(all(test, target_os = "macos"))]
mod macos_tests {
    use rustix::termios::ControlModes as C;

    use super::*;
    use crate::pty::testing::{Program, settings};

    /// Decides [`FORWARD_FRAMING`] on macOS: does a pty keep 7E1?
    #[test]
    #[ignore = "unverified platform behaviour: run on a Mac to decide FORWARD_FRAMING"]
    fn macos_keeps_framing() {
        let pty = Pty::open(&settings(9600)).unwrap();
        let program = Program::open(&pty);
        let _ = program.try_change(|t| {
            t.control_modes -= C::CSIZE;
            t.control_modes |= C::CS7 | C::PARENB;
        });
        let s = PtySettings::decode(&program.termios(), true);
        assert_eq!(
            (s.data_bits, s.parity),
            (Some(DataBits::Seven), Some(Parity::Even))
        );
    }
}
