//! The UI sync channel of a CarPlay Ultra session with a car-maker theme:
//! the phone and the head unit agree on the instrument layout the theme's
//! cluster shows. Each message is an OPACK frame naming the panel, with a
//! binary plist header in its `data` (a type, the session and the two
//! sequence numbers) and, for a command, another plist inside that.
//!
//! The head unit draws no cluster, so it keeps to the minimum: it answers
//! the reset, lets the phone change the layout, asks for the theme's
//! layout when one was provisioned, acknowledges every command and hands
//! the focus straight back (it has nothing of its own to focus).

use crate::opack;
use crate::plist::{self, Value};

pub const FEATURE: &str = "uiSync";
pub const INFO_KEY: &str = "uiSyncInfo";
pub const DATASTREAM_UUID: &str = "07D9F906-8D64-4B54-A808-A20BCA2C51C2";

const RESET: i64 = 0;
const RESET_COMPLETE: i64 = 1;
const COMMAND: i64 = 2;
const COMMAND_ACK: i64 = 3;
const VERSION: i64 = 2;

const START_LAYOUT_TRANSITION: i64 = 0x03;
const REQUEST_LAYOUT: i64 = 0x05;
const GIVE_FOCUS: i64 = 0x07;
const REQUEST_FOCUS: i64 = 0x08;
const ALLOW_LAYOUT_CHANGE: i64 = 0x11;
const FOCUS_TOKEN: i64 = 4711;
const DARK: i64 = 1;

/// A layout to ask for, from the theme.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Layout {
    pub id: String,
    pub palette: String,
}

#[derive(Default)]
pub struct UiSync {
    session: i64,
    next_tx: i64,
    last_rx: i64,
    ready: bool,
    awaiting_reset_complete: bool,
    phone_id: Option<Value>,
    version: i64,
    /// The frame's other fields (the panel), echoed on every answer.
    panel: Vec<(opack::Value, opack::Value)>,
    wanted: Option<Layout>,
}

impl UiSync {
    pub fn new(wanted: Option<Layout>) -> Self {
        Self {
            wanted,
            ..Self::default()
        }
    }

    /// One frame from the phone; the frames that answer it.
    pub fn handle(&mut self, message: &[u8]) -> Vec<Vec<u8>> {
        let Some(opack::Value::Dict(frame)) = opack::decode(message) else {
            log::warn!(
                "carplay: ui sync: a {} byte frame is not OPACK",
                message.len()
            );
            return Vec::new();
        };
        let data = opack::Value::str("data");
        let Some(header) = frame
            .iter()
            .find(|(k, _)| *k == data)
            .and_then(|(_, v)| v.as_data())
            .and_then(plist::decode)
        else {
            return Vec::new();
        };
        self.panel = frame.into_iter().filter(|(k, _)| *k != data).collect();
        let int = |k| header.get(k).and_then(Value::as_i64);
        let mut out = Vec::new();
        match int("type") {
            Some(RESET) => {
                if self.awaiting_reset_complete && self.next_tx == 2 {
                    return out;
                }
                self.session += 1;
                self.next_tx = 1;
                self.last_rx = 0;
                self.ready = false;
                self.awaiting_reset_complete = true;
                self.phone_id = header.get("id").cloned();
                let fields = self.with_id(vec![("version", Value::Int(VERSION))]);
                out.push(self.message(RESET, None, fields));
            }
            _ if int("ssn") != Some(self.session) => {}
            Some(RESET_COMPLETE) if self.awaiting_reset_complete => {
                self.awaiting_reset_complete = false;
                self.ready = true;
                self.version = int("version").unwrap_or(1).min(VERSION);
                self.last_rx = int("psn").unwrap_or(0);
                let mut fields = vec![("version", Value::Int(self.version))];
                if self.version >= 2 {
                    fields = self.with_id(fields);
                }
                out.push(self.message(RESET_COMPLETE, None, fields));
                out.push(self.message(COMMAND, Some(allow_layout_change()), Vec::new()));
                if let Some(layout) = &self.wanted {
                    let request = request_layout(layout);
                    out.push(self.message(COMMAND, Some(request), Vec::new()));
                }
                out.push(self.message(COMMAND, Some(give_focus()), Vec::new()));
                log::info!(
                    "carplay: ui sync: the channel is up (version {})",
                    self.version
                );
            }
            Some(COMMAND) if self.ready => {
                self.last_rx = int("psn").unwrap_or(self.last_rx);
                let wants_focus = follow_phone(&header);
                if header.bool_of("noAck") != Some(true) {
                    out.push(self.message(COMMAND_ACK, None, Vec::new()));
                }
                if wants_focus {
                    out.push(self.message(COMMAND, Some(give_focus()), Vec::new()));
                }
            }
            Some(COMMAND_ACK) => self.last_rx = int("psn").unwrap_or(self.last_rx),
            _ => {}
        }
        out
    }

    fn with_id<'a>(&self, mut fields: Vec<(&'a str, Value)>) -> Vec<(&'a str, Value)> {
        if let Some(id) = &self.phone_id {
            fields.push(("id", id.clone()));
        }
        fields
    }

    fn message(&mut self, kind: i64, data: Option<Value>, extra: Vec<(&str, Value)>) -> Vec<u8> {
        let mut header = Value::dict()
            .with("type", Value::Int(kind))
            .with("ssn", Value::Int(self.session))
            .with("psn", Value::Int(self.next_tx))
            .with("asn", Value::Int(self.last_rx));
        for (k, v) in extra {
            header.set(k, v);
        }
        if let Some(data) = data {
            header.set("data", Value::Data(plist::encode(&data)));
        }
        self.next_tx += 1;
        let mut frame = self.panel.clone();
        frame.push((
            opack::Value::str("data"),
            opack::Value::Data(plist::encode(&header)),
        ));
        opack::encode(&opack::Value::Dict(frame))
    }
}

/// What the phone's command says, logged; true when it asks for the focus.
fn follow_phone(header: &Value) -> bool {
    let Some(command) = header
        .get("data")
        .and_then(Value::as_data)
        .and_then(plist::decode)
    else {
        return false;
    };
    let text = |k: &str| {
        command
            .get("data")
            .and_then(|d| d.get(k))
            .and_then(Value::as_str)
    };
    match command.get("type").and_then(Value::as_i64) {
        Some(START_LAYOUT_TRANSITION) => {
            log::info!(
                "carplay: ui sync: the phone shows layout {} ({})",
                text("layoutID").unwrap_or("?"),
                text("paletteID").unwrap_or("-")
            );
            false
        }
        Some(REQUEST_FOCUS) => true,
        Some(other) => {
            log::debug!("carplay: ui sync: phone command {other:#x}");
            false
        }
        None => false,
    }
}

fn give_focus() -> Value {
    Value::dict().with("type", Value::Int(GIVE_FOCUS)).with(
        "data",
        Value::dict().with("tokenID", Value::Int(FOCUS_TOKEN)),
    )
}

fn allow_layout_change() -> Value {
    Value::dict()
        .with("type", Value::Int(ALLOW_LAYOUT_CHANGE))
        .with("data", Value::dict().with("allow", Value::Bool(true)))
}

fn request_layout(layout: &Layout) -> Value {
    Value::dict().with("type", Value::Int(REQUEST_LAYOUT)).with(
        "data",
        Value::dict()
            .with("layoutID", Value::str(&layout.id))
            .with("paletteID", Value::str(&layout.palette))
            .with("appearanceMode", Value::Int(DARK))
            .with("fadeInNewLayout", Value::Bool(true))
            .with("fadeOutOldLayout", Value::Bool(true))
            .with("activeComponents", Value::dict()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn framed(header: Value) -> Vec<u8> {
        opack::encode(&opack::dict([
            ("displayUUID", opack::Value::str("Cluster_Display")),
            ("data", opack::Value::Data(plist::encode(&header))),
        ]))
    }

    fn header_of(out: &[u8]) -> Value {
        let frame = opack::decode(out).unwrap();
        assert_eq!(
            frame.get("displayUUID").and_then(opack::Value::as_str),
            Some("Cluster_Display")
        );
        plist::decode(frame.get("data").and_then(opack::Value::as_data).unwrap()).unwrap()
    }

    fn command_of(h: &Value) -> Value {
        plist::decode(h.get("data").and_then(Value::as_data).unwrap()).unwrap()
    }

    fn h(fields: &[(&str, i64)]) -> Value {
        let mut v = Value::dict();
        for (k, n) in fields {
            v.set(k, Value::Int(*n));
        }
        v
    }

    #[test]
    fn the_head_unit_answers_a_reset_and_asks_for_its_layout() {
        let layout = Layout {
            id: "0D38B526".into(),
            palette: "blue".into(),
        };
        let mut sync = UiSync::new(Some(layout));
        let reset = framed(
            h(&[
                ("type", 0),
                ("ssn", 1),
                ("psn", 1),
                ("asn", 0),
                ("version", 3),
            ])
            .with("id", Value::str("89858A51")),
        );
        let out = sync.handle(&reset);
        assert_eq!(out.len(), 1);
        let a = header_of(&out[0]);
        assert_eq!(
            (
                a.int_or("type", -1),
                a.int_or("ssn", -1),
                a.int_or("psn", -1)
            ),
            (0, 1, 1)
        );
        assert_eq!(a.int_or("version", 0), 2);
        assert_eq!(a.str_or("id", ""), "89858A51");

        let complete = framed(h(&[("type", 1), ("ssn", 1), ("psn", 1), ("version", 3)]));
        let out = sync.handle(&complete);
        assert_eq!(out.len(), 4);
        let a = header_of(&out[0]);
        assert_eq!((a.int_or("type", -1), a.int_or("asn", -1)), (1, 1));
        let allow = command_of(&header_of(&out[1]));
        assert_eq!(allow.int_or("type", 0), ALLOW_LAYOUT_CHANGE);
        let request = command_of(&header_of(&out[2]));
        assert_eq!(request.int_or("type", 0), REQUEST_LAYOUT);
        assert_eq!(
            request.get("data").unwrap().str_or("layoutID", ""),
            "0D38B526"
        );
        let focus = command_of(&header_of(&out[3]));
        assert_eq!(focus.int_or("type", 0), GIVE_FOCUS);
        assert_eq!(header_of(&out[3]).int_or("psn", 0), 5);

        // A command is acknowledged; a focus request answered with focus.
        let ask = Value::dict()
            .with("type", Value::Int(REQUEST_FOCUS))
            .with("data", Value::dict());
        let from_phone = framed(
            h(&[("type", 2), ("ssn", 1), ("psn", 2)])
                .with("data", Value::Data(plist::encode(&ask))),
        );
        let out = sync.handle(&from_phone);
        assert_eq!(out.len(), 2);
        let a = header_of(&out[0]);
        assert_eq!((a.int_or("type", -1), a.int_or("asn", -1)), (3, 2));
        assert_eq!(
            command_of(&header_of(&out[1])).int_or("type", 0),
            GIVE_FOCUS
        );

        // Another session's frame and garbage are not answered.
        assert!(
            sync.handle(&framed(h(&[("type", 2), ("ssn", 9), ("psn", 3)])))
                .is_empty()
        );
        assert!(sync.handle(&[0xff]).is_empty());
        assert!(sync.handle(&opack::encode(&opack::dict([]))).is_empty());
    }

    #[test]
    fn without_a_layout_the_head_unit_only_allows_and_gives_focus() {
        let mut sync = UiSync::new(None);
        sync.handle(&framed(h(&[("type", 0), ("ssn", 1), ("psn", 1)])));
        let out = sync.handle(&framed(h(&[("type", 1), ("ssn", 1), ("psn", 1)])));
        assert_eq!(out.len(), 3);
        let a = header_of(&out[0]);
        assert_eq!(a.get("id"), None);
        assert_eq!(a.int_or("version", 0), 1, "the phone's version, no higher");
    }
}
