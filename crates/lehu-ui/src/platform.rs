// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! The Slint platform under the screens: one software-rendered window of the
//! panel's size and no event loop of its own -- the main loop ticks it.
//! Slint keeps one platform per thread for the life of the process, so the
//! first `Osd` sets it and the next (only in tests) finds it set.

use std::cell::RefCell;
use std::rc::Rc;

use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
use slint::platform::{Platform, PlatformError, WindowAdapter};
use slint::{ComponentHandle, PhysicalSize};

use crate::AppWindow;

thread_local! {
    // The window the platform made for the component being created, for
    // `create` to pick up: `set_platform` gives back no handle to it.
    static CREATED: RefCell<Option<Rc<MinimalSoftwareWindow>>> = const { RefCell::new(None) };
}

struct OsdPlatform;

impl Platform for OsdPlatform {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
        // ReusedBuffer: the frame always holds the complete picture, so the
        // renderer repaints only what changed and says where.
        let window = MinimalSoftwareWindow::new(RepaintBufferType::ReusedBuffer);
        CREATED.with(|slot| *slot.borrow_mut() = Some(window.clone()));
        Ok(window)
    }
}

/// The screens, shown, and the window they render into, `width` x `height`
/// pixels.
pub(crate) fn create(
    width: u32,
    height: u32,
) -> Result<(AppWindow, Rc<MinimalSoftwareWindow>), PlatformError> {
    // Already set means an earlier Osd on this thread did it: fine.
    if slint::platform::set_platform(Box::new(OsdPlatform)).is_err() {
        log::debug!("screens: the Slint platform was already set");
    }
    let ui = AppWindow::new()?;
    let window = CREATED
        .with(|slot| slot.borrow_mut().take())
        .ok_or_else(|| PlatformError::Other("the platform made no window".into()))?;
    window.set_size(PhysicalSize::new(width, height));
    ui.show()?;
    Ok((ui, window))
}
