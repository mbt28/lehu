// SPDX-License-Identifier: GPL-3.0-or-later
//! Compiles the V4L2 shim. Only the kernel's own headers are needed, which
//! every Linux sysroot has; there is nothing to gate.

fn main() {
    println!("cargo:rerun-if-changed=csrc/camera.c");
    cc::Build::new()
        .file("csrc/camera.c")
        .warnings(true)
        .compile("lehu_camera_v4l2");
}
