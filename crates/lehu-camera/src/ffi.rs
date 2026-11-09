// SPDX-License-Identifier: GPL-3.0-or-later
//! The V4L2 shim (`csrc/camera.c`) behind a device handle. Every call maps
//! -errno to `Err(errno)`.

use std::ffi::{CStr, CString, c_char, c_int};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct CtrlInfo {
    pub id: u32,
    pub min: i32,
    pub max: i32,
    pub step: i32,
    pub def: i32,
    pub kind: u32,
    pub flags: u32,
    pub name: [c_char; 32],
}

const CTRL_TYPE_MENU: u32 = 3;
const CTRL_FLAG_DISABLED: u32 = 1;
const CTRL_FLAG_READ_ONLY: u32 = 4;

impl CtrlInfo {
    pub fn is_menu(&self) -> bool {
        self.kind == CTRL_TYPE_MENU
    }
    pub fn disabled(&self) -> bool {
        self.flags & CTRL_FLAG_DISABLED != 0
    }
    pub fn read_only(&self) -> bool {
        self.flags & CTRL_FLAG_READ_ONLY != 0
    }
    fn name(&self) -> String {
        // SAFETY: the shim NUL-terminates the name.
        unsafe { CStr::from_ptr(self.name.as_ptr()) }
            .to_string_lossy()
            .into_owned()
    }
}

unsafe extern "C" {
    fn lehu_cam_open(path: *const c_char) -> c_int;
    fn lehu_cam_enum_input(
        fd: c_int,
        index: u32,
        name: *mut c_char,
        len: usize,
        has_std: *mut c_int,
        no_signal: *mut c_int,
    ) -> c_int;
    fn lehu_cam_set_input(fd: c_int, index: c_int) -> c_int;
    fn lehu_cam_query_std(fd: c_int, std: *mut u64) -> c_int;
    fn lehu_cam_set_std(fd: c_int, std: u64) -> c_int;
    fn lehu_cam_get_std(fd: c_int, std: *mut u64) -> c_int;
    fn lehu_cam_query_ctrl(fd: c_int, id: u32, out: *mut CtrlInfo) -> c_int;
    fn lehu_cam_next_ctrl(fd: c_int, id: u32, out: *mut CtrlInfo) -> c_int;
    fn lehu_cam_query_menu(fd: c_int, id: u32, index: u32, name: *mut c_char, len: usize) -> c_int;
    fn lehu_cam_get_ctrl(fd: c_int, id: u32, value: *mut i32) -> c_int;
    fn lehu_cam_set_ctrl(fd: c_int, id: u32, value: i32) -> c_int;
    fn lehu_cam_set_nv12(fd: c_int, width: *mut u32, height: *mut u32, pitch: *mut u32) -> c_int;
    fn lehu_cam_reqbufs(fd: c_int, count: u32) -> c_int;
    fn lehu_cam_expbuf(fd: c_int, index: u32) -> c_int;
    fn lehu_cam_qbuf(fd: c_int, index: u32) -> c_int;
    fn lehu_cam_dqbuf(fd: c_int, index: *mut u32) -> c_int;
    fn lehu_cam_stream(fd: c_int, on: c_int) -> c_int;
}

fn check(r: c_int) -> Result<c_int, i32> {
    if r < 0 { Err(-r) } else { Ok(r) }
}

pub struct InputInfo {
    pub name: String,
    pub has_std: bool,
    pub no_signal: bool,
}

/// An open capture device.
pub struct Dev(OwnedFd);

impl AsRawFd for Dev {
    fn as_raw_fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }
}

impl Dev {
    pub fn open(path: &str) -> Result<Self, i32> {
        let c = CString::new(path).map_err(|_| libc::EINVAL)?;
        // SAFETY: a NUL-terminated path; the shim returns a descriptor or
        // -errno.
        let fd = check(unsafe { lehu_cam_open(c.as_ptr()) })?;
        // SAFETY: a fresh descriptor we own.
        Ok(Self(unsafe { OwnedFd::from_raw_fd(fd) }))
    }

    pub fn enum_input(&self, index: u32) -> Option<InputInfo> {
        let mut name = [0 as c_char; 32];
        let (mut has_std, mut no_signal) = (0, 0);
        // SAFETY: the buffer and the ints are locals of the stated sizes.
        let r = unsafe {
            lehu_cam_enum_input(
                self.as_raw_fd(),
                index,
                name.as_mut_ptr(),
                name.len(),
                &mut has_std,
                &mut no_signal,
            )
        };
        check(r).ok()?;
        // SAFETY: the shim NUL-terminates the name.
        let name = unsafe { CStr::from_ptr(name.as_ptr()) }
            .to_string_lossy()
            .into_owned();
        Some(InputInfo {
            name,
            has_std: has_std != 0,
            no_signal: no_signal != 0,
        })
    }

    pub fn set_input(&self, index: i32) -> Result<(), i32> {
        // SAFETY: plain ints.
        check(unsafe { lehu_cam_set_input(self.as_raw_fd(), index) }).map(|_| ())
    }

    pub fn query_std(&self) -> Option<u64> {
        let mut std = 0;
        // SAFETY: a local u64 for the result.
        check(unsafe { lehu_cam_query_std(self.as_raw_fd(), &mut std) }).ok()?;
        Some(std)
    }

    pub fn set_std(&self, std: u64) -> Result<(), i32> {
        // SAFETY: plain ints.
        check(unsafe { lehu_cam_set_std(self.as_raw_fd(), std) }).map(|_| ())
    }

    pub fn get_std(&self) -> Option<u64> {
        let mut std = 0;
        // SAFETY: a local u64 for the result.
        check(unsafe { lehu_cam_get_std(self.as_raw_fd(), &mut std) }).ok()?;
        Some(std)
    }

    pub fn query_ctrl(&self, id: u32) -> Option<CtrlInfo> {
        let mut info = CtrlInfo::default();
        // SAFETY: a local struct of the shim's layout.
        check(unsafe { lehu_cam_query_ctrl(self.as_raw_fd(), id, &mut info) }).ok()?;
        Some(info)
    }

    /// A control by name (driver-private ones have no fixed id).
    pub fn find_ctrl(&self, name: &str) -> Option<u32> {
        let mut id = 0;
        loop {
            let mut info = CtrlInfo::default();
            // SAFETY: a local struct of the shim's layout.
            check(unsafe { lehu_cam_next_ctrl(self.as_raw_fd(), id, &mut info) }).ok()?;
            if info.name() == name {
                return Some(info.id);
            }
            id = info.id;
        }
    }

    pub fn query_menu(&self, id: u32, index: u32) -> Option<String> {
        let mut name = [0 as c_char; 32];
        // SAFETY: a local buffer of the stated length.
        let r = unsafe {
            lehu_cam_query_menu(self.as_raw_fd(), id, index, name.as_mut_ptr(), name.len())
        };
        check(r).ok()?;
        // SAFETY: the shim NUL-terminates the name.
        Some(
            unsafe { CStr::from_ptr(name.as_ptr()) }
                .to_string_lossy()
                .into_owned(),
        )
    }

    pub fn get_ctrl(&self, id: u32) -> Option<i32> {
        let mut value = 0;
        // SAFETY: a local for the result.
        check(unsafe { lehu_cam_get_ctrl(self.as_raw_fd(), id, &mut value) }).ok()?;
        Some(value)
    }

    pub fn set_ctrl(&self, id: u32, value: i32) -> Result<(), i32> {
        // SAFETY: plain ints.
        check(unsafe { lehu_cam_set_ctrl(self.as_raw_fd(), id, value) }).map(|_| ())
    }

    /// (width, height, pitch) of the NV12 format set.
    pub fn set_nv12(&self) -> Result<(u32, u32, u32), i32> {
        let (mut w, mut h, mut p) = (0, 0, 0);
        // SAFETY: three locals for the results.
        check(unsafe { lehu_cam_set_nv12(self.as_raw_fd(), &mut w, &mut h, &mut p) })?;
        Ok((w, h, p))
    }

    /// How many buffers the driver gave.
    pub fn reqbufs(&self, count: u32) -> Result<u32, i32> {
        // SAFETY: plain ints.
        check(unsafe { lehu_cam_reqbufs(self.as_raw_fd(), count) }).map(|n| n as u32)
    }

    pub fn expbuf(&self, index: u32) -> Result<OwnedFd, i32> {
        // SAFETY: plain ints; the result is a descriptor we own.
        let fd = check(unsafe { lehu_cam_expbuf(self.as_raw_fd(), index) })?;
        // SAFETY: a fresh dma-buf descriptor.
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }

    pub fn qbuf(&self, index: u32) -> Result<(), i32> {
        qbuf(self.as_raw_fd(), index)
    }

    pub fn stream(&self, on: bool) -> Result<(), i32> {
        // SAFETY: plain ints.
        check(unsafe { lehu_cam_stream(self.as_raw_fd(), c_int::from(on)) }).map(|_| ())
    }
}

/// For the capture thread, which holds the raw descriptor.
pub fn qbuf(fd: RawFd, index: u32) -> Result<(), i32> {
    // SAFETY: plain ints.
    check(unsafe { lehu_cam_qbuf(fd, index) }).map(|_| ())
}

pub fn dqbuf(fd: RawFd) -> Result<u32, i32> {
    let mut index = 0;
    // SAFETY: a local for the result.
    check(unsafe { lehu_cam_dqbuf(fd, &mut index) })?;
    Ok(index)
}
