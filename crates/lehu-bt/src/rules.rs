// SPDX-License-Identifier: GPL-3.0-or-later
//! The agent's rules as pure state, so they are tested with a clock of the
//! test's choosing: which device the user is pairing right now (the only
//! input device the agent says yes to), and what each kind of request from
//! BlueZ gets.

/// After `Pair()` succeeds the device stays the pairing target this long: a
/// classic keyboard opens its HID channels right away, before Trusted lands.
pub const TARGET_GRACE_MS: u64 = 10_000;

/// The pairing the user started from the screens, if any.
#[derive(Debug, Default)]
pub struct Window {
    target: Option<String>,
    pair_pending: bool,
    user_cancelled: bool,
    /// The target stays one until this moment once `Pair()` has answered.
    target_until: u64,
}

impl Window {
    pub fn new() -> Self {
        Self::default()
    }

    /// `Pair()` goes out for `path`.
    pub fn begin(&mut self, path: &str) {
        self.target = Some(path.to_string());
        self.pair_pending = true;
        self.user_cancelled = false;
        self.target_until = 0;
    }

    pub fn target(&self) -> Option<&str> {
        self.target.as_deref()
    }

    pub fn pair_pending(&self) -> bool {
        self.pair_pending
    }

    pub fn user_cancelled(&self) -> bool {
        self.user_cancelled
    }

    /// The device the user is pairing. After the user's Cancel the device
    /// is no longer welcome: a request still on its way is refused instead
    /// of opening a new dialog.
    pub fn is_target(&self, path: &str, now: u64) -> bool {
        self.target.as_deref() == Some(path)
            && !self.user_cancelled
            && (self.pair_pending || now < self.target_until)
    }

    /// `Pair()` answered well: the window stays open for the grace.
    pub fn paired(&mut self, now: u64) {
        self.pair_pending = false;
        self.target_until = now + TARGET_GRACE_MS;
    }

    /// `Pair()` answered at all (well or not).
    pub fn pair_answered(&mut self) {
        self.pair_pending = false;
    }

    /// The user cancelled the dialog.
    pub fn cancel(&mut self) {
        self.user_cancelled = true;
    }

    /// The pairing ends without its `Pair()` reply (the device went, the
    /// user forgot it, bluetoothd left); a late reply is ignored.
    pub fn abandon(&mut self) {
        self.pair_pending = false;
        self.target = None;
    }

    /// Drop the target once its grace is over; true when it was dropped.
    pub fn expire(&mut self, now: u64) -> bool {
        if self.target.is_some() && !self.pair_pending && now >= self.target_until {
            self.target = None;
            return true;
        }
        false
    }

    /// When the window next changes by itself, in `now_ms` time.
    pub fn deadline(&self) -> Option<u64> {
        (self.target.is_some() && !self.pair_pending).then_some(self.target_until)
    }
}

/// What BlueZ's agent asks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Request {
    /// RequestConfirmation: the device shows the same passkey.
    Confirmation,
    /// RequestAuthorization: just-works pairing.
    Authorization,
    /// DisplayPasskey: the device types what we show.
    DisplayPasskey,
    DisplayPinCode,
    /// RequestPinCode: a legacy device wants a PIN.
    RequestPinCode,
    /// RequestPasskey: we would have to type; never.
    RequestPasskey,
    /// AuthorizeService for a UUID; `hid` when it is HID or HID over GATT.
    AuthorizeService {
        hid: bool,
    },
}

/// What the agent does with a request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    /// An empty reply.
    Accept,
    /// `org.bluez.Error.Rejected`.
    Reject,
    /// Open the dialog; the reply waits for the user (a confirmation) or is
    /// owed at once (a code shown).
    Prompt,
    /// Cannot be refused by a reply: drop the link.
    Disconnect,
    /// Answer with a PIN: a random one shown to a keyboard, "0000" for
    /// anything else.
    Pin,
}

/// The device's standing with the agent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Standing {
    /// The device the user is pairing from the screens.
    pub target: bool,
    /// An input device (keyboard, remote).
    pub hid: bool,
    /// Phones pair over Bluetooth in this mode (wireless CarPlay or
    /// Android Auto). Wired modes ran no agent before this manager, so
    /// BlueZ refused every phone there; the agent keeps that.
    pub phone_pairing: bool,
}

pub fn decide(request: Request, s: Standing) -> Decision {
    let phones = if s.phone_pairing {
        Decision::Accept
    } else {
        Decision::Reject
    };
    match request {
        Request::Confirmation if s.target => Decision::Prompt,
        Request::Confirmation | Request::Authorization if s.target => Decision::Accept,
        Request::Confirmation | Request::Authorization if s.hid => Decision::Reject,
        Request::Confirmation | Request::Authorization => phones,
        Request::AuthorizeService { .. } if s.target => Decision::Accept,
        Request::AuthorizeService { hid: true } => Decision::Reject,
        Request::AuthorizeService { hid: false } => phones,
        Request::DisplayPasskey if s.target => Decision::Prompt,
        Request::DisplayPasskey if s.hid => Decision::Disconnect,
        Request::DisplayPasskey => Decision::Accept,
        Request::DisplayPinCode if s.target => Decision::Prompt,
        Request::DisplayPinCode => Decision::Accept,
        Request::RequestPinCode if s.target => Decision::Pin,
        Request::RequestPinCode => Decision::Reject,
        Request::RequestPasskey => Decision::Reject,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TARGET: Standing = Standing {
        target: true,
        hid: true,
        phone_pairing: true,
    };
    const OTHER_HID: Standing = Standing {
        target: false,
        hid: true,
        phone_pairing: true,
    };
    const PHONE: Standing = Standing {
        target: false,
        hid: false,
        phone_pairing: true,
    };
    const PHONE_WIRED: Standing = Standing {
        target: false,
        hid: false,
        phone_pairing: false,
    };

    #[test]
    fn the_table_from_the_docs() {
        use Decision::*;
        use Request::*;
        let rows = [
            (Confirmation, Prompt, Reject, Accept),
            (Authorization, Accept, Reject, Accept),
            (DisplayPasskey, Prompt, Disconnect, Accept),
            (DisplayPinCode, Prompt, Accept, Accept),
            (RequestPinCode, Pin, Reject, Reject),
            (RequestPasskey, Reject, Reject, Reject),
            (AuthorizeService { hid: true }, Accept, Reject, Reject),
            (AuthorizeService { hid: false }, Accept, Accept, Accept),
        ];
        for (req, target, other, phone) in rows {
            assert_eq!(decide(req, TARGET), target, "{req:?} target");
            assert_eq!(decide(req, OTHER_HID), other, "{req:?} other hid");
            assert_eq!(decide(req, PHONE), phone, "{req:?} phone");
        }
    }

    #[test]
    fn wired_modes_refuse_phones_but_not_their_own_target() {
        use Decision::*;
        use Request::*;
        for req in [
            Confirmation,
            Authorization,
            AuthorizeService { hid: false },
            RequestPinCode,
        ] {
            assert_eq!(decide(req, PHONE_WIRED), Reject, "{req:?}");
        }
        // Displays are answered either way: bluetoothd shows them anyway.
        assert_eq!(decide(DisplayPasskey, PHONE_WIRED), Accept);
        let wired_target = Standing {
            phone_pairing: false,
            ..TARGET
        };
        assert_eq!(decide(Confirmation, wired_target), Prompt);
        assert_eq!(decide(AuthorizeService { hid: true }, wired_target), Accept);
    }

    #[test]
    fn the_window_lasts_the_pairing_and_a_grace_after() {
        let mut w = Window::new();
        assert!(!w.is_target("/d", 0));
        w.begin("/d");
        assert!(w.is_target("/d", 0));
        assert!(!w.is_target("/e", 0));
        assert!(w.pair_pending());
        assert_eq!(w.deadline(), None);
        w.paired(1000);
        assert!(!w.pair_pending());
        assert!(w.is_target("/d", 1000 + TARGET_GRACE_MS - 1));
        assert!(!w.is_target("/d", 1000 + TARGET_GRACE_MS));
        assert_eq!(w.deadline(), Some(1000 + TARGET_GRACE_MS));
        assert!(!w.expire(1000 + TARGET_GRACE_MS - 1));
        assert!(w.expire(1000 + TARGET_GRACE_MS));
        assert_eq!(w.target(), None);
    }

    #[test]
    fn a_cancel_closes_the_window_and_abandon_forgets_it() {
        let mut w = Window::new();
        w.begin("/d");
        w.cancel();
        assert!(w.user_cancelled());
        assert!(!w.is_target("/d", 0));
        assert_eq!(w.target(), Some("/d"), "the reply is still awaited");
        w.pair_answered();
        assert!(!w.pair_pending());
        w.begin("/d");
        assert!(!w.user_cancelled(), "a new pairing starts clean");
        w.abandon();
        assert_eq!(w.target(), None);
        assert!(!w.pair_pending());
    }
}
