//! Holding the picture after a dropped frame until a keyframe arrives.
//!
//! A dropped frame breaks the chain of references: every frame after the
//! gap predicts from a picture the decoder never had, smearing garbage over
//! moving content until a keyframe. When the protocol can ask for one, the
//! decoder skips to it and holds the last good picture meanwhile, but not
//! for long: a phone that ignores the request must cost a stutter, not a
//! freeze. Parameter sets always go through (CarPlay sends them as a unit
//! of their own and the keyframe after them needs the new ones).
//!
//! Every gap needs its own keyframe, but while a request is outstanding
//! further drops are the same backlog and add nothing; a request is
//! repeated after a second in case it was lost. There is no flush here, on
//! purpose: `avcodec_flush_buffers` on v4l2-request allocates a second
//! cedrus pool, which this board's CMA cannot hold. An IDR resets the
//! references by itself.

use crate::annexb::Kinds;

/// How long the picture is held for the keyframe before decoding through
/// the gap anyway.
pub const TIMEOUT_MS: u64 = 300;
/// A keyframe asked for is asked for again after this.
pub const REASK_MS: u64 = 1000;

/// What to do with the unit at hand.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Gate {
    Decode,
    /// Not a keyframe, not parameter sets, and the wait is not over.
    Skip,
}

/// Why a resync ended, for the log.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum End {
    Keyframe { waited_ms: u64 },
    Timeout { waited_ms: u64 },
}

#[derive(Default)]
pub struct Resync {
    /// When the hold began; None when not resyncing.
    since: Option<u64>,
    /// When a keyframe was last asked for; 0 never.
    asked_ms: u64,
}

impl Resync {
    pub fn active(&self) -> bool {
        self.since.is_some()
    }

    /// Frames were dropped at `now_ms`. True when a keyframe should be asked
    /// for now.
    pub fn on_drop(&mut self, now_ms: u64) -> bool {
        if self.since.is_none() {
            self.since = Some(now_ms);
        }
        if self.asked_ms == 0 || now_ms.saturating_sub(self.asked_ms) >= REASK_MS {
            self.asked_ms = now_ms;
            return true;
        }
        false
    }

    /// Decide for a unit of `kinds` at `now_ms`; `Some(End)` when the
    /// resync ended with it.
    pub fn gate(&mut self, now_ms: u64, kinds: Kinds) -> (Gate, Option<End>) {
        let Some(since) = self.since else {
            return (Gate::Decode, None);
        };
        let waited_ms = now_ms.saturating_sub(since);
        if !kinds.keyframe && !kinds.params && waited_ms < TIMEOUT_MS {
            return (Gate::Skip, None);
        }
        if kinds.keyframe {
            self.since = None;
            return (Gate::Decode, Some(End::Keyframe { waited_ms }));
        }
        if !kinds.params {
            self.since = None;
            return (Gate::Decode, Some(End::Timeout { waited_ms }));
        }
        // Parameter sets are decoded and the resync goes on.
        (Gate::Decode, None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const P: Kinds = Kinds {
        keyframe: false,
        params: false,
    };
    const KEY: Kinds = Kinds {
        keyframe: true,
        params: false,
    };
    const PARAMS: Kinds = Kinds {
        keyframe: false,
        params: true,
    };

    #[test]
    fn without_a_drop_everything_decodes() {
        let mut r = Resync::default();
        assert_eq!(r.gate(1000, P), (Gate::Decode, None));
        assert!(!r.active());
    }

    #[test]
    fn a_drop_holds_until_the_keyframe() {
        let mut r = Resync::default();
        assert!(r.on_drop(1000), "asks at once");
        assert!(r.active());
        assert_eq!(r.gate(1010, P), (Gate::Skip, None));
        assert_eq!(r.gate(1200, P), (Gate::Skip, None));
        assert_eq!(
            r.gate(1250, KEY),
            (Gate::Decode, Some(End::Keyframe { waited_ms: 250 }))
        );
        assert!(!r.active());
        assert_eq!(r.gate(1260, P), (Gate::Decode, None));
    }

    #[test]
    fn parameter_sets_pass_and_the_hold_goes_on() {
        let mut r = Resync::default();
        r.on_drop(1000);
        assert_eq!(r.gate(1050, PARAMS), (Gate::Decode, None));
        assert!(r.active());
        assert_eq!(r.gate(1060, P), (Gate::Skip, None));
        let both = Kinds {
            keyframe: true,
            params: true,
        };
        assert_eq!(
            r.gate(1070, both),
            (Gate::Decode, Some(End::Keyframe { waited_ms: 70 }))
        );
    }

    #[test]
    fn the_hold_ends_after_the_timeout() {
        let mut r = Resync::default();
        r.on_drop(1000);
        assert_eq!(r.gate(1299, P), (Gate::Skip, None));
        assert_eq!(
            r.gate(1300, P),
            (Gate::Decode, Some(End::Timeout { waited_ms: 300 }))
        );
        assert!(!r.active());
        // Parameter sets after the timeout do not end anything either way.
        r.on_drop(2000);
        assert_eq!(r.gate(2400, PARAMS), (Gate::Decode, None));
        assert!(r.active());
    }

    #[test]
    fn the_request_is_repeated_after_a_second_only() {
        let mut r = Resync::default();
        assert!(r.on_drop(1000));
        assert!(!r.on_drop(1500), "the same backlog");
        assert!(!r.on_drop(1999));
        assert!(r.on_drop(2000));
        // A resync that ended and a later drop within the second: no new
        // request either, the earlier one still stands.
        r.gate(2100, KEY);
        assert!(!r.on_drop(2500));
        assert!(r.active());
        assert!(r.on_drop(3000));
    }

    #[test]
    fn time_going_backwards_does_not_underflow() {
        let mut r = Resync::default();
        r.on_drop(5000);
        assert_eq!(r.gate(4000, P), (Gate::Skip, None));
        assert!(!r.on_drop(4000));
    }
}
