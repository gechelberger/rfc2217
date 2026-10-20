//! Telnet option negotiation state (RFC 1143 "Q method", without the
//! disable-request states since neither side ever asks to turn an option off),
//! plus a loop breaker for peers that keep re-enabling an option.
//!
//! Storage is fixed: only an option a policy accepts can leave the `No` state,
//! and both halves accept a subset of BINARY, SGA and COM-PORT-OPTION, so only
//! those three get a slot. Every other option is refused without any state.
//! Replies are returned, not written: the owner queues them.

use crate::log::debug;
use crate::telnet::{DO, DONT, IAC, OPT_BINARY, OPT_COM_PORT, OPT_SGA, WILL, WONT};

/// The options that have a slot, in slot order.
const SLOTS: [u8; 3] = [OPT_BINARY, OPT_SGA, OPT_COM_PORT];

/// Policy bit: accept BINARY.
pub(crate) const ACCEPT_BINARY: u8 = 1 << 0;
/// Policy bit: accept SGA.
pub(crate) const ACCEPT_SGA: u8 = 1 << 1;
/// Policy bit: accept COM-PORT-OPTION.
pub(crate) const ACCEPT_COM_PORT: u8 = 1 << 2;

/// Peer-initiated enables allowed per option and direction. Later requests
/// are refused, which ends the WILL/DONT ping-pong that contradictory bursts
/// can start between two Q-method implementations. Never reset.
const MAX_PEER_ENABLES: u8 = 8;

/// One negotiation command to send: `IAC <verb> <option>`.
pub(crate) type Reply = [u8; 3];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Q {
    #[default]
    No,
    WantYes,
    Yes,
}

/// One direction of one option.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct Side {
    q: Q,
    /// `No -> Yes` transitions caused by a peer request.
    peer_enables: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct Sides {
    us: Side,
    them: Side,
}

/// Which side of an option a command is about.
#[derive(Debug, Clone, Copy)]
enum Dir {
    /// We perform the option (`WILL`/`WONT` from us, `DO`/`DONT` from the peer).
    Us,
    /// The peer performs it (`DO`/`DONT` from us, `WILL`/`WONT` from the peer).
    Them,
}

impl Dir {
    /// The verb we send to enable this side.
    const fn yes(self) -> u8 {
        match self {
            Self::Us => WILL,
            Self::Them => DO,
        }
    }

    /// The verb we send to refuse or disable this side.
    const fn no(self) -> u8 {
        match self {
            Self::Us => WONT,
            Self::Them => DONT,
        }
    }

    /// Position of this side's 3-bit mask in the policy byte.
    const fn shift(self) -> u32 {
        match self {
            Self::Us => 0,
            Self::Them => 3,
        }
    }
}

fn slot(option: u8) -> Option<usize> {
    SLOTS.iter().position(|&o| o == option)
}

/// Tracks, per option, whether we and the peer have it enabled, and produces
/// the replies the Telnet rules require.
#[derive(Debug)]
pub(crate) struct Negotiator {
    table: [Sides; SLOTS.len()],
    /// The `accept_us` mask in bits 0-2, `accept_them` in bits 3-5.
    policy: u8,
}

impl Negotiator {
    /// `accept_us`: options we agree to perform when the peer asks (`DO`);
    /// `accept_them`: options we let the peer perform (`WILL`). Both are
    /// sets of `ACCEPT_*` bits. Answers to our own requests are always taken.
    pub(crate) fn new(accept_us: u8, accept_them: u8) -> Self {
        let all = ACCEPT_BINARY | ACCEPT_SGA | ACCEPT_COM_PORT;
        debug_assert!(
            accept_us & !all == 0 && accept_them & !all == 0,
            "policy bits for options without a slot"
        );
        Self {
            table: [Sides::default(); SLOTS.len()],
            policy: accept_us | (accept_them << Dir::Them.shift()),
        }
    }

    fn side(&self, dir: Dir, option: u8) -> Option<Side> {
        let sides = self.table[slot(option)?];
        Some(match dir {
            Dir::Us => sides.us,
            Dir::Them => sides.them,
        })
    }

    fn side_mut(&mut self, dir: Dir, option: u8) -> Option<&mut Side> {
        let sides = &mut self.table[slot(option)?];
        Some(match dir {
            Dir::Us => &mut sides.us,
            Dir::Them => &mut sides.them,
        })
    }

    fn accepts(&self, dir: Dir, option: u8) -> bool {
        slot(option).is_some_and(|i| (self.policy >> dir.shift()) & (1 << i) != 0)
    }

    pub(crate) fn us(&self, option: u8) -> Q {
        self.side(Dir::Us, option).map_or(Q::No, |s| s.q)
    }

    pub(crate) fn them(&self, option: u8) -> Q {
        self.side(Dir::Them, option).map_or(Q::No, |s| s.q)
    }

    /// Either side has the option active. (Each half reads the one side of
    /// COM-PORT-OPTION its state follows, so only the tests ask this.)
    #[cfg(test)]
    pub(crate) fn active(&self, option: u8) -> bool {
        self.us(option) == Q::Yes || self.them(option) == Q::Yes
    }

    /// Either side is still waiting for an answer. (The client's state reads
    /// our side alone, so only the tests ask this.)
    #[cfg(test)]
    pub(crate) fn pending(&self, option: u8) -> bool {
        self.us(option) == Q::WantYes || self.them(option) == Q::WantYes
    }

    /// Ask to enable the option on our side (`WILL`).
    pub(crate) fn request_us(&mut self, option: u8) -> Option<Reply> {
        self.request(Dir::Us, option)
    }

    /// Ask the peer to enable the option (`DO`).
    pub(crate) fn request_them(&mut self, option: u8) -> Option<Reply> {
        self.request(Dir::Them, option)
    }

    /// The peer sent `DO <option>`.
    pub(crate) fn on_do(&mut self, option: u8) -> Option<Reply> {
        self.on_enable(Dir::Us, option)
    }

    /// The peer sent `DONT <option>`.
    pub(crate) fn on_dont(&mut self, option: u8) -> Option<Reply> {
        self.on_disable(Dir::Us, option)
    }

    /// The peer sent `WILL <option>`.
    pub(crate) fn on_will(&mut self, option: u8) -> Option<Reply> {
        self.on_enable(Dir::Them, option)
    }

    /// The peer sent `WONT <option>`.
    pub(crate) fn on_wont(&mut self, option: u8) -> Option<Reply> {
        self.on_disable(Dir::Them, option)
    }

    fn request(&mut self, dir: Dir, option: u8) -> Option<Reply> {
        debug_assert!(slot(option).is_some(), "option {option} has no slot");
        let side = self.side_mut(dir, option)?;
        (side.q == Q::No).then(|| {
            side.q = Q::WantYes;
            [IAC, dir.yes(), option]
        })
    }

    fn on_enable(&mut self, dir: Dir, option: u8) -> Option<Reply> {
        let accept = self.accepts(dir, option);
        let refuse = Some([IAC, dir.no(), option]);
        let Some(side) = self.side_mut(dir, option) else {
            return refuse;
        };
        match side.q {
            Q::No if !accept => refuse,
            Q::No if side.peer_enables >= MAX_PEER_ENABLES => {
                debug!("refusing option {option}: the peer enabled it {MAX_PEER_ENABLES} times");
                refuse
            }
            Q::No => {
                side.q = Q::Yes;
                side.peer_enables += 1;
                Some([IAC, dir.yes(), option])
            }
            Q::WantYes => {
                side.q = Q::Yes;
                None
            }
            Q::Yes => None,
        }
    }

    fn on_disable(&mut self, dir: Dir, option: u8) -> Option<Reply> {
        let side = self.side_mut(dir, option)?;
        match side.q {
            Q::Yes => {
                side.q = Q::No;
                Some([IAC, dir.no(), option])
            }
            Q::WantYes => {
                side.q = Q::No;
                None
            }
            Q::No => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: u8 = ACCEPT_BINARY | ACCEPT_SGA | ACCEPT_COM_PORT;

    #[test]
    fn request_then_accept() {
        let mut n = Negotiator::new(ACCEPT_COM_PORT, 0);
        assert_eq!(n.request_us(44), Some([IAC, WILL, 44]));
        assert!(n.pending(44));
        assert_eq!(n.on_do(44), None, "no reply to an answer");
        assert!(n.active(44));
        assert!(!n.pending(44));
    }

    #[test]
    fn peer_offer_is_answered_once() {
        let mut n = Negotiator::new(0, ACCEPT_COM_PORT);
        assert_eq!(n.on_will(44), Some([IAC, DO, 44]));
        assert_eq!(n.them(44), Q::Yes);
        assert_eq!(n.on_will(44), None);
        assert_eq!(n.on_will(1), Some([IAC, DONT, 1]));
    }

    #[test]
    fn refusal_clears_pending() {
        let mut n = Negotiator::new(ALL, ALL);
        assert_eq!(n.request_them(44), Some([IAC, DO, 44]));
        assert_eq!(n.on_wont(44), None);
        assert!(!n.active(44) && !n.pending(44));
        assert_eq!(n.request_us(3), Some([IAC, WILL, 3]));
        assert_eq!(n.on_dont(3), None);
        assert_eq!(n.us(3), Q::No);
    }

    #[test]
    fn do_for_unsupported_option_is_refused_with_wont() {
        let mut n = Negotiator::new(ALL, ALL);
        for option in [1, 24, 31, 34] {
            assert_eq!(n.on_do(option), Some([IAC, WONT, option]));
            assert_eq!(n.us(option), Q::No);
        }
        // A slot option that the policy does not accept is refused the same way.
        let mut n = Negotiator::new(ACCEPT_BINARY, ALL);
        assert_eq!(n.on_do(44), Some([IAC, WONT, 44]));
        assert_eq!(n.us(44), Q::No);
    }

    #[test]
    fn dont_from_yes_acks_with_wont_once() {
        let mut n = Negotiator::new(ALL, ALL);
        assert_eq!(n.on_do(0), Some([IAC, WILL, 0]));
        assert_eq!(n.on_dont(0), Some([IAC, WONT, 0]));
        assert_eq!(n.us(0), Q::No);
        assert_eq!(n.on_dont(0), None);
    }

    #[test]
    fn wont_from_yes_acks_with_dont_once() {
        let mut n = Negotiator::new(ALL, ALL);
        assert_eq!(n.on_will(0), Some([IAC, DO, 0]));
        assert_eq!(n.on_wont(0), Some([IAC, DONT, 0]));
        assert_eq!(n.them(0), Q::No);
        assert_eq!(n.on_wont(0), None);
    }

    #[test]
    fn wont_in_no_state_is_silent() {
        let mut n = Negotiator::new(ALL, ALL);
        for option in [0, 1, 3, 44, 255] {
            assert_eq!(n.on_wont(option), None);
            assert_eq!(n.on_dont(option), None);
        }
        assert_eq!(n.table, Negotiator::new(ALL, ALL).table);
    }

    #[test]
    fn repeat_request_is_noop() {
        let mut n = Negotiator::new(ALL, ALL);
        assert_eq!(n.request_us(44), Some([IAC, WILL, 44]));
        assert_eq!(n.request_us(44), None);
        assert_eq!(n.request_them(44), Some([IAC, DO, 44]));
        assert_eq!(n.request_them(44), None);
        assert_eq!(n.on_do(44), None);
        assert_eq!(n.on_will(44), None);
        // Once enabled, a request or a peer repeat changes nothing.
        assert_eq!(n.request_us(44), None);
        assert_eq!(n.on_do(44), None);
        assert_eq!(n.on_will(44), None);
        assert!(n.us(44) == Q::Yes && n.them(44) == Q::Yes);
    }

    #[test]
    fn ninth_peer_enable_is_refused() {
        let mut n = Negotiator::new(ALL, ALL);
        // Answers to our own requests do not count.
        for _ in 0..2 * MAX_PEER_ENABLES {
            assert_eq!(n.request_us(0), Some([IAC, WILL, 0]));
            assert_eq!(n.on_do(0), None);
            assert_eq!(n.on_dont(0), Some([IAC, WONT, 0]));
        }
        for _ in 0..MAX_PEER_ENABLES {
            assert_eq!(n.on_do(0), Some([IAC, WILL, 0]));
            assert_eq!(n.on_dont(0), Some([IAC, WONT, 0]));
        }
        assert_eq!(n.on_do(0), Some([IAC, WONT, 0]));
        assert_eq!(n.on_do(0), Some([IAC, WONT, 0]));
        assert_eq!(n.us(0), Q::No);

        // The counters are per option and per direction.
        assert_eq!(n.on_will(0), Some([IAC, DO, 0]));
        assert_eq!(n.on_do(3), Some([IAC, WILL, 3]));

        // Answers to our own requests are still taken after the limit.
        assert_eq!(n.request_us(0), Some([IAC, WILL, 0]));
        assert_eq!(n.on_do(0), None);
        assert_eq!(n.us(0), Q::Yes);
    }

    #[test]
    fn direction_policy_is_respected() {
        let mut n = Negotiator::new(ACCEPT_COM_PORT, ACCEPT_BINARY);
        assert_eq!(n.on_will(44), Some([IAC, DONT, 44]));
        assert_eq!(n.on_do(44), Some([IAC, WILL, 44]));
        assert_eq!(n.on_do(0), Some([IAC, WONT, 0]));
        assert_eq!(n.on_will(0), Some([IAC, DO, 0]));
        assert_eq!(n.on_do(3), Some([IAC, WONT, 3]));
        assert_eq!(n.on_will(3), Some([IAC, DONT, 3]));
        assert!(n.us(44) == Q::Yes && n.them(44) == Q::No);
        assert!(n.us(0) == Q::No && n.them(0) == Q::Yes);
    }

    #[test]
    fn unknown_options_need_no_slot() {
        // Nothing accepted: all 256 options are refused without state.
        let mut n = Negotiator::new(0, 0);
        for option in 0..=255 {
            assert_eq!(n.on_do(option), Some([IAC, WONT, option]));
            assert_eq!(n.on_will(option), Some([IAC, DONT, option]));
        }
        assert_eq!(n.table, Negotiator::new(0, 0).table);

        // Everything accepted: every option without a slot is still refused,
        // as often as the peer asks, and never counts.
        let mut n = Negotiator::new(ALL, ALL);
        for _ in 0..MAX_PEER_ENABLES + 1 {
            for option in (0..=255).filter(|o| slot(*o).is_none()) {
                assert_eq!(n.on_do(option), Some([IAC, WONT, option]));
                assert_eq!(n.on_will(option), Some([IAC, DONT, option]));
            }
        }
        assert_eq!(n.table, Negotiator::new(ALL, ALL).table);
    }
}
