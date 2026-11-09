// SPDX-License-Identifier: GPL-3.0-or-later
//! Bluetooth through BlueZ, for the whole program: one manager thread that
//! runs the pairing agent, keeps the input devices the screens list, and
//! registers the RFCOMM profiles the phones' wireless bootstraps run on.
//! libdbus is spoken directly (`ffi`, loaded at run time) over a private
//! system-bus connection; nothing here blocks a caller on D-Bus.

mod bus;
pub mod device;
mod ffi;
mod manager;
pub mod prompt;
pub mod rfcomm;
pub mod rules;

pub use device::Address;
pub use manager::{AdapterAddress, BtEvent, Device, Manager, Snapshot};
pub use prompt::{Prompt, PromptKind};
pub use rfcomm::{RfcommConnection, RfcommHandle, RfcommProfile, RfcommStream, Role};
