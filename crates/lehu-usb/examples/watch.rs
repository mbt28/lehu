// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! Print the devices on the bus and every attach and detach until Ctrl-C:
//! `watch` alone, or `watch reset BUS ADDRESS` to reset one port.

use std::time::Duration;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() == 4 && args[1] == "reset" {
        let (bus, address) = (args[2].parse::<u8>(), args[3].parse::<u8>());
        let Some(device) = lehu_usb::scan().ok().and_then(|all| {
            all.into_iter()
                .find(|d| Ok(d.bus) == bus && Ok(d.address) == address)
        }) else {
            eprintln!("no such device");
            std::process::exit(1);
        };
        match lehu_usb::reset(&device) {
            Ok(()) => println!("reset {device}"),
            Err(e) => eprintln!("reset {device}: {e}"),
        }
        return;
    }
    // Kept alive until the process ends: the watcher thread does the work.
    let _watcher = lehu_usb::Watcher::start(|event| match event {
        lehu_usb::Event::Attached(d) => println!("attached {d} ({:?})", d.role()),
        lehu_usb::Event::Detached(d) => println!("detached {d}"),
    })
    .expect("uevent socket");
    loop {
        std::thread::sleep(Duration::from_secs(3600));
    }
}
