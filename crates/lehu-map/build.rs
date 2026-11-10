// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! Compiles the video engine's JPEG shim. Only the kernel's own headers are
//! needed, which every Linux sysroot has; there is nothing to gate.

fn main() {
    println!("cargo:rerun-if-changed=csrc/jpeg.c");
    cc::Build::new()
        .file("csrc/jpeg.c")
        .warnings(true)
        .compile("lehu_map_jpeg");
}
