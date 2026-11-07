//! The vehicle state protocol of a CarPlay Ultra session, at its minimum.
//! The head unit describes itself as accessories made of services and
//! their characteristics (each an instance id, a type, a format, who may
//! write it); the phone reads the description, follows values and writes
//! the ones it owns. Every message is OPACK on one of two data streams,
//! one for high-priority values.
//!
//! This head unit has no car behind it: no CAN bus, no climate, no fuel,
//! no speed. So it describes only what holds no car data: route sharing,
//! with the car side declining it (it has no navigation of its own to take
//! the route), and one line in the phone's vehicle settings naming the
//! head unit. Every other value the phone asks for is answered as not
//! held, every command as not there.

use std::collections::{BTreeMap, BTreeSet};

use crate::opack::{self, Value, dict};
use crate::plist;

pub const FEATURE: &str = "vehicleStateProtocol";
pub const HIGH_PRIORITY_DATASTREAM_UUID: &str = "FF4A6720-F2BE-4F56-A3E1-DB3B4E37D634";
pub const DATASTREAM_UUID: &str = "3E2F3C61-AAD0-42CB-A8AA-BF22186DA62E";
pub const INFO_KEY: &str = "vehicleStateProtocolInfo";

const PROTOCOL_VERSION: &str = "1.0";
const PLUGIN: &str = "LEHU";
const PLUGIN_ID: i64 = 0;
const ACCESSORY_VERSION: &str = "1.0";
const HIGH_PRIORITY: i64 = 1;
const MAX_LENGTH: i64 = 65535;
/// The answer for a value not held, or one the phone may not write.
const NO_VALUE: i64 = -6727;
const NO_SUCH_COMMAND: i64 = -6735;
const WRITTEN: i64 = 0;
/// A value the phone writes is kept up to this size; a bigger one is
/// taken and dropped (nothing here reads it back).
const MAX_KEPT: usize = 4096;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Format {
    Bool = 0,
    Uint8 = 1,
    Uint32 = 3,
    String = 10,
    Dict = 12,
    Array = 13,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Owner {
    Phone,
    Car,
}

struct Characteristic {
    iid: i64,
    kind: i64,
    name: &'static str,
    format: Format,
    owner: Owner,
    mutable: bool,
    max: i64,
    initial: Option<Value>,
}

fn phone(iid: i64, kind: i64, name: &'static str, format: Format) -> Characteristic {
    let max = if format == Format::Uint32 {
        i64::from(u32::MAX)
    } else {
        0
    };
    Characteristic {
        iid,
        kind,
        name,
        format,
        owner: Owner::Phone,
        mutable: true,
        max,
        initial: None,
    }
}

fn car(iid: i64, kind: i64, name: &'static str, format: Format, initial: Value) -> Characteristic {
    Characteristic {
        iid,
        kind,
        name,
        format,
        owner: Owner::Car,
        mutable: true,
        max: 0,
        initial: Some(initial),
    }
}

impl Characteristic {
    fn up_to(self, max: i64) -> Self {
        Self { max, ..self }
    }

    fn fixed(self) -> Self {
        Self {
            mutable: false,
            ..self
        }
    }

    fn describe(&self) -> Value {
        let mut d = vec![
            ("iid", Value::Int(self.iid)),
            ("type", Value::Int(self.kind)),
            ("format", Value::Int(self.format as i64)),
            ("mutable", Value::Bool(self.mutable)),
            ("writable", Value::Bool(self.owner == Owner::Phone)),
            ("oemWritable", Value::Bool(self.owner == Owner::Car)),
            ("priority", Value::Int(HIGH_PRIORITY)),
        ];
        match self.format {
            Format::Uint8 | Format::Uint32 => d.extend([
                ("minimumValue", Value::Int(0)),
                ("maximumValue", Value::Int(self.max)),
                ("stepValue", Value::Int(1)),
            ]),
            Format::String | Format::Dict | Format::Array => {
                d.push(("maximumLength", Value::Int(MAX_LENGTH)));
            }
            Format::Bool => {}
        }
        // The phone can clear what it wrote, except its enumerations.
        if self.owner == Owner::Phone && self.format != Format::Uint8 {
            d.push(("supportsInvalid", Value::Bool(true)));
        }
        if let Some(v) = &self.initial {
            d.push(("initialValue", v.clone()));
        }
        Value::Dict(d.into_iter().map(|(k, v)| (Value::str(k), v)).collect())
    }
}

struct Service {
    iid: i64,
    kind: i64,
    characteristics: Vec<Characteristic>,
}

struct Accessory {
    iid: i64,
    kind: i64,
    services: Vec<Service>,
}

/// Route sharing: the phone may hand the car its route; this car says it
/// takes none (`vehicleEnabled` false), so the phone keeps the route and
/// sends no legs.
fn navigation(name: &str) -> Accessory {
    use Format::*;
    Accessory {
        iid: 256,
        kind: 0x0E00_0002,
        services: vec![
            Service {
                iid: 257,
                kind: 0x1E00_0104,
                characteristics: vec![
                    phone(258, 0x4500_0103, "applicationEnabled", Bool),
                    phone(259, 0x4500_0112, "currentLegIndex", Uint32),
                    phone(260, 0x4500_0109, "legs", Array),
                    phone(261, 0x4500_010D, "sharingState", Uint8).up_to(4),
                    phone(262, 0x4500_0104, "userEnabled", Bool),
                    car(
                        263,
                        0x4500_010E,
                        "userVisibleReason",
                        String,
                        Value::str(name),
                    )
                    .fixed(),
                    car(264, 0x4500_0105, "vehicleEnabled", Bool, Value::Bool(false)),
                ],
            },
            Service {
                iid: 265,
                kind: 0x1E00_0102,
                characteristics: vec![
                    phone(266, 0x4500_0108, "destination", Dict),
                    phone(267, 0x4500_0106, "geodeticSystem", Uint8).up_to(2),
                    phone(268, 0x4500_0107, "origin", Dict),
                    phone(269, 0x4500_0110, "rerouteReason", Uint8).up_to(6),
                    phone(270, 0x4500_0101, "routeState", Uint8).up_to(255),
                    phone(271, 0x4500_0102, "userVisibleApplicationName", String),
                ],
            },
            Service {
                iid: 272,
                kind: 0x1E00_0103,
                characteristics: vec![
                    car(273, 0x4500_010C, "routeSource", Uint8, Value::Int(0)).up_to(5),
                    car(
                        274,
                        0x4500_010A,
                        "userVisibleNavigationSystemName",
                        String,
                        Value::str(name),
                    )
                    .fixed(),
                    car(
                        275,
                        0x4500_0111,
                        "userVisibleVehicleBrand",
                        String,
                        Value::str(name),
                    )
                    .fixed(),
                ],
            },
        ],
    }
}

const SECTION: &str = "LEHU";

/// One read-only line in the phone's vehicle settings: the head unit and
/// its version, in a section of its own.
fn settings(version: &str) -> Accessory {
    use Format::*;
    let line =
        |iid: i64, kind: i64, name, format, v: Value| car(iid, kind, name, format, v).fixed();
    Accessory {
        iid: 1280,
        kind: 0x0500_0001,
        services: vec![
            Service {
                iid: 1281,
                kind: 0x1600_0015,
                characteristics: vec![
                    line(
                        1282,
                        0x3000_0019,
                        "settingIdentifier",
                        String,
                        Value::str("LehuVersion"),
                    ),
                    line(
                        1283,
                        0x3000_0001,
                        "settingLabel",
                        String,
                        Value::str("Version"),
                    ),
                    line(
                        1284,
                        0x3000_0017,
                        "settingValue",
                        String,
                        Value::str(version),
                    ),
                    line(1285, 0x3600_0006, "settingsCategory", Uint8, Value::Int(0)).up_to(15),
                    line(
                        1286,
                        0x3000_005E,
                        "settingSymbol",
                        String,
                        Value::str("info.circle"),
                    ),
                    line(1287, 0x3000_0003, "settingSortOrder", Uint8, Value::Int(0)).up_to(100),
                    line(
                        1288,
                        0x3600_0028,
                        "settingSection",
                        String,
                        Value::str(SECTION),
                    ),
                ],
            },
            Service {
                iid: 1297,
                kind: 0x1600_0020,
                characteristics: vec![
                    line(
                        1298,
                        0x3000_0019,
                        "sectionIdentifier",
                        String,
                        Value::str(SECTION),
                    ),
                    line(1299, 0x3000_0003, "sectionSortOrder", Uint8, Value::Int(0)).up_to(100),
                    line(
                        1300,
                        0x3600_0004,
                        "sectionName",
                        String,
                        Value::str(SECTION),
                    ),
                ],
            },
        ],
    }
}

pub struct VehicleState {
    accessories: Vec<Accessory>,
    values: BTreeMap<i64, Value>,
    followed: BTreeSet<i64>,
}

impl VehicleState {
    pub fn new(name: &str, version: &str) -> Self {
        let accessories = vec![navigation(name), settings(version)];
        let values = accessories
            .iter()
            .flat_map(|a| &a.services)
            .flat_map(|s| &s.characteristics)
            .filter_map(|c| Some((c.iid, c.initial.clone()?)))
            .collect();
        Self {
            accessories,
            values,
            followed: BTreeSet::new(),
        }
    }

    fn characteristic(&self, iid: i64) -> Option<&Characteristic> {
        self.accessories
            .iter()
            .flat_map(|a| &a.services)
            .flat_map(|s| &s.characteristics)
            .find(|c| c.iid == iid)
    }

    fn accessory_list(&self) -> Value {
        let accessory = |a: &Accessory| {
            let services = a.services.iter().map(|s| {
                let characteristics = s.characteristics.iter().map(Characteristic::describe);
                dict([
                    ("iid", Value::Int(s.iid)),
                    ("type", Value::Int(s.kind)),
                    ("characteristics", Value::Array(characteristics.collect())),
                ])
            });
            dict([
                ("iid", Value::Int(a.iid)),
                ("type", Value::Int(a.kind)),
                ("version", Value::str(ACCESSORY_VERSION)),
                ("services", Value::Array(services.collect())),
            ])
        };
        Value::Array(self.accessories.iter().map(accessory).collect())
    }

    /// The `/info` entry that says the head unit speaks the protocol and
    /// what it describes.
    pub fn info_entry(&self) -> plist::Value {
        let plugin = dict([
            ("name", Value::str(PLUGIN)),
            ("pluginID", Value::Int(PLUGIN_ID)),
            ("accessories", self.accessory_list()),
        ]);
        to_plist(&dict([
            ("protocolVersion", Value::str(PROTOCOL_VERSION)),
            ("pluginCount", Value::Int(1)),
            (
                "pluginMapping",
                Value::Dict(vec![(Value::str(PLUGIN), Value::Int(PLUGIN_ID))]),
            ),
            ("pluginConfigs", Value::Array(vec![plugin])),
        ]))
    }

    /// One message from the phone; the answers go back on its stream.
    pub fn handle(&mut self, message: &[u8]) -> Vec<Vec<u8>> {
        let Some(outer) = opack::decode(message) else {
            log::warn!(
                "carplay: vehicle state: a {} byte message is not OPACK",
                message.len()
            );
            return Vec::new();
        };
        let plugin = outer.get("pluginID").and_then(Value::as_int);
        let request = outer
            .get("pluginData")
            .and_then(Value::as_data)
            .and_then(opack::decode);
        let Some(request) = request.filter(|_| plugin == Some(PLUGIN_ID)) else {
            log::warn!("carplay: vehicle state: a message for plugin {plugin:?}, dropped");
            return Vec::new();
        };
        let transaction = request.get("transactionID").cloned().unwrap_or(Value::Null);
        let values = request.get("values");
        let answer = match request.get("command").and_then(Value::as_str).unwrap_or("") {
            "configRequest" => {
                log::info!("carplay: vehicle state: the phone reads the description");
                dict([
                    ("command", Value::str("configResponse")),
                    ("transactionID", transaction),
                    ("values", dict([("accessories", self.accessory_list())])),
                ])
            }
            "registerRequest" => {
                let iids = values.and_then(Value::as_array).unwrap_or_default();
                self.follow(iids, transaction)
            }
            "writeRequest" => {
                let written = values.and_then(Value::as_dict).unwrap_or_default();
                self.write(written, transaction)
            }
            "controlRequest" => {
                let commands = values.and_then(Value::as_dict).unwrap_or_default();
                let refused: Vec<(Value, Value)> = commands
                    .iter()
                    .map(|(k, _)| (k.clone(), Value::Int(NO_SUCH_COMMAND)))
                    .collect();
                log::info!(
                    "carplay: vehicle state: the phone sent {} commands, none here",
                    refused.len()
                );
                dict([
                    ("command", Value::str("controlResponse")),
                    ("transactionID", transaction),
                    ("errors", Value::Dict(refused)),
                ])
            }
            other => {
                log::info!("carplay: vehicle state: the phone sent {other:?}, not handled");
                return Vec::new();
            }
        };
        vec![wrap(&answer)]
    }

    fn follow(&mut self, iids: &[Value], transaction: Value) -> Value {
        let mut held = Vec::new();
        let mut missing = Vec::new();
        for iid in iids.iter().filter_map(Value::as_int) {
            if self.characteristic(iid).is_some() {
                self.followed.insert(iid);
            }
            match self.values.get(&iid) {
                Some(v) => held.push((Value::Int(iid), v.clone())),
                None => missing.push((Value::Int(iid), Value::Int(NO_VALUE))),
            }
        }
        log::info!(
            "carplay: vehicle state: the phone follows {} values, {} of them held",
            iids.len(),
            held.len()
        );
        let mut answer = vec![
            (Value::str("command"), Value::str("registerResponse")),
            (Value::str("transactionID"), transaction),
            (Value::str("values"), Value::Dict(held)),
        ];
        if !missing.is_empty() {
            answer.push((Value::str("errors"), Value::Dict(missing)));
        }
        Value::Dict(answer)
    }

    fn write(&mut self, written: &[(Value, Value)], transaction: Value) -> Value {
        let mut results = Vec::new();
        for (key, v) in written {
            let Some(iid) = key.as_int() else { continue };
            let name = match self.characteristic(iid) {
                Some(c) if c.owner == Owner::Phone => c.name,
                _ => {
                    log::info!("carplay: vehicle state: the phone may not write {iid}");
                    results.push((Value::Int(iid), Value::Int(NO_VALUE)));
                    continue;
                }
            };
            log::debug!("carplay: vehicle state: the phone wrote {name}");
            results.push((Value::Int(iid), Value::Int(WRITTEN)));
            if v.weight() <= MAX_KEPT {
                self.values.insert(iid, v.clone());
            } else {
                self.values.remove(&iid);
            }
        }
        dict([
            ("command", Value::str("writeResponse")),
            ("transactionID", transaction),
            ("errors", Value::Dict(results)),
        ])
    }
}

/// The frame every message travels in: the plugin and its data.
fn wrap(inner: &Value) -> Vec<u8> {
    opack::encode(&dict([
        ("pluginID", Value::Int(PLUGIN_ID)),
        ("pluginData", Value::Data(opack::encode(inner))),
    ]))
}

/// The description again as a property list, for `/info`.
fn to_plist(v: &Value) -> plist::Value {
    match v {
        Value::Null => plist::Value::Data(Vec::new()),
        Value::Bool(b) => plist::Value::Bool(*b),
        Value::Int(n) => plist::Value::Int(*n),
        Value::Real(f) => plist::Value::Real(*f),
        Value::Str(s) => plist::Value::Str(s.clone()),
        Value::Data(d) => plist::Value::Data(d.clone()),
        Value::Uuid(u) => plist::Value::Data(u.to_vec()),
        Value::Array(items) => plist::Value::Array(items.iter().map(to_plist).collect()),
        Value::Dict(entries) => plist::Value::Dict(
            entries
                .iter()
                .map(|(k, v)| {
                    let key = match k {
                        Value::Str(s) => s.clone(),
                        Value::Int(n) => n.to_string(),
                        _ => String::new(),
                    };
                    (key, to_plist(v))
                })
                .collect(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(command: &str, values: Option<Value>) -> Vec<u8> {
        let mut inner = vec![
            (Value::str("command"), Value::str(command)),
            (Value::str("transactionID"), Value::Int(77)),
        ];
        if let Some(v) = values {
            inner.push((Value::str("values"), v));
        }
        opack::encode(&dict([
            ("pluginID", Value::Int(0)),
            (
                "pluginData",
                Value::Data(opack::encode(&Value::Dict(inner))),
            ),
        ]))
    }

    fn answer(out: &[Vec<u8>]) -> Value {
        assert_eq!(out.len(), 1);
        let outer = opack::decode(&out[0]).unwrap();
        assert_eq!(outer.get("pluginID"), Some(&Value::Int(0)));
        opack::decode(outer.get("pluginData").and_then(Value::as_data).unwrap()).unwrap()
    }

    #[test]
    fn the_phone_reads_follows_and_writes() {
        let mut v = VehicleState::new("LEHU", "1.2.3");
        let info = v.info_entry();
        assert_eq!(info.str_or("protocolVersion", ""), "1.0");
        let configs = info
            .get("pluginConfigs")
            .and_then(plist::Value::as_array)
            .unwrap();
        let accessories = configs[0]
            .get("accessories")
            .and_then(plist::Value::as_array);
        assert_eq!(accessories.map(<[_]>::len), Some(2));

        let a = answer(&v.handle(&request("configRequest", None)));
        assert_eq!(
            a.get("command").and_then(Value::as_str),
            Some("configResponse")
        );
        assert_eq!(a.get("transactionID"), Some(&Value::Int(77)));

        let follow = Value::Array(vec![Value::Int(264), Value::Int(1284), Value::Int(9999)]);
        let a = answer(&v.handle(&request("registerRequest", Some(follow))));
        let held = a.get("values").and_then(Value::as_dict).unwrap();
        assert!(
            held.contains(&(Value::Int(264), Value::Bool(false))),
            "route sharing declined"
        );
        assert!(held.contains(&(Value::Int(1284), Value::str("1.2.3"))));
        let errors = a.get("errors").and_then(Value::as_dict).unwrap();
        assert_eq!(errors, [(Value::Int(9999), Value::Int(NO_VALUE))]);

        let writes = Value::Dict(vec![
            (Value::Int(262), Value::Bool(true)),
            (Value::Int(264), Value::Bool(true)),
            (Value::Int(260), Value::Data(vec![0; 10_000])),
        ]);
        let a = answer(&v.handle(&request("writeRequest", Some(writes))));
        let results = a.get("errors").and_then(Value::as_dict).unwrap();
        assert_eq!(
            results,
            [
                (Value::Int(262), Value::Int(WRITTEN)),
                (Value::Int(264), Value::Int(NO_VALUE)),
                (Value::Int(260), Value::Int(WRITTEN)),
            ]
        );
        assert_eq!(
            v.values.get(&264),
            Some(&Value::Bool(false)),
            "the car's own"
        );
        assert_eq!(v.values.get(&262), Some(&Value::Bool(true)));
        assert_eq!(v.values.get(&260), None, "too big to keep");

        let commands = Value::Dict(vec![(Value::Int(4110), Value::Null)]);
        let a = answer(&v.handle(&request("controlRequest", Some(commands))));
        assert_eq!(
            a.get("errors").and_then(Value::as_dict).unwrap(),
            [(Value::Int(4110), Value::Int(NO_SUCH_COMMAND))]
        );
    }

    #[test]
    fn strange_messages_get_no_answer() {
        let mut v = VehicleState::new("LEHU", "1");
        assert!(v.handle(&[0xff]).is_empty());
        assert!(v.handle(&request("dance", None)).is_empty());
        let other_plugin = opack::encode(&dict([
            ("pluginID", Value::Int(3)),
            ("pluginData", Value::Data(Vec::new())),
        ]));
        assert!(v.handle(&other_plugin).is_empty());
        // Values of the wrong shape are taken as none.
        let a = answer(&v.handle(&request("registerRequest", Some(Value::Int(1)))));
        assert_eq!(a.get("values"), Some(&Value::Dict(Vec::new())));
    }
}
