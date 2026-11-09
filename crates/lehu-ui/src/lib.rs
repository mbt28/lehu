// SPDX-License-Identifier: GPL-3.0-or-later
//! The screens: the head unit's own Slint UI, ported from FastCarPlay as it
//! is -- the knob-first lists with Back on top, the rows picked in place, the
//! on-screen keyboard, the camera column, the volume bar -- and the
//! controller that drives them.
//!
//! [`Osd`] renders the screens in software into one RGB565 frame and hands
//! the rows that changed to a [`Surface`] (the DRM overlay plane); touch and
//! keys reach it from any thread through [`Input`]; everything it needs from
//! the rest of the program goes through [`Host`]. The platform under it (the
//! plane, evdev) is the program's.

slint::include_modules!();

pub mod host;
mod input;
pub mod keys;
pub mod osd;
mod platform;
#[cfg(test)]
pub(crate) mod test_host;
pub mod text;

pub use host::{
    ART_SIZE, Art, AudioOutput, BtDevice, CameraLevel, CameraRequest, Host, KnownDevice, Media,
    MediaKey, NavArrow, NavCard, Power, Prompt, PromptKind, SystemNames, Update, UpdateOutcome,
    UpdateStage,
};
pub use input::Input;
pub use keys::OsdKey;
pub use osd::{Osd, Rect, Surface};

/// The pixel the screens are rendered in, as the plane takes it.
pub use slint::platform::software_renderer::Rgb565Pixel;

/// How long until Slint's next timer or animation step wants a tick;
/// `None` when nothing is pending. The main loop's poll timeout.
pub fn next_timer_due() -> Option<std::time::Duration> {
    slint::platform::duration_until_next_timer_update()
}
