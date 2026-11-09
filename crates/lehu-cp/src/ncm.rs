// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! The USB network link the phone brings up in configuration 6: the
//! kernel's cdc_ncm binds it and gives it a link-local IPv6 address, which
//! is what the phone is told to connect back to. A head unit has no
//! network manager to bring the interface up, so this does.

use std::ffi::CStr;
use std::fs;
use std::path::Path;

/// The network interface the phone's NCM function got, by its sysfs
/// interface directories (`<name>:6.N/net/<iface>`): the one bound to
/// cdc_ncm. Configuration 6 also carries the tethering function, which
/// ipheth binds as an `ethN` with no carrier: not that one. "usb0" when
/// sysfs does not say.
pub fn interface_of(sysname: &str) -> String {
    let mut any = None;
    for n in 2..=6 {
        let iface = Path::new("/sys/bus/usb/devices").join(format!("{sysname}:6.{n}"));
        let Ok(entries) = fs::read_dir(iface.join("net")) else {
            continue;
        };
        let Some(name) = entries
            .flatten()
            .next()
            .map(|e| e.file_name().to_string_lossy().into_owned())
        else {
            continue;
        };
        let driver = fs::read_link(iface.join("driver"))
            .ok()
            .and_then(|p| p.file_name().map(|f| f.to_string_lossy().into_owned()));
        if driver.as_deref() == Some("cdc_ncm") {
            return name;
        }
        any.get_or_insert(name);
    }
    any.unwrap_or_else(|| "usb0".to_string())
}

/// The interface's link-local IPv6 address, once it has one.
pub fn link_local(iface: &str) -> Option<String> {
    let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs fills a list we free below; every pointer read is
    // checked for null and the family before the cast.
    unsafe {
        if libc::getifaddrs(&mut list) != 0 {
            return None;
        }
        let mut found = None;
        let mut p = list;
        while !p.is_null() {
            let a = &*p;
            if !a.ifa_addr.is_null()
                && i32::from((*a.ifa_addr).sa_family) == libc::AF_INET6
                && CStr::from_ptr(a.ifa_name).to_str() == Ok(iface)
            {
                let s6 = &*(a.ifa_addr as *const libc::sockaddr_in6);
                let bytes = s6.sin6_addr.s6_addr;
                if bytes[0] == 0xfe && bytes[1] & 0xc0 == 0x80 {
                    found = Some(std::net::Ipv6Addr::from(bytes).to_string());
                    break;
                }
            }
            p = a.ifa_next;
        }
        libc::freeifaddrs(list);
        found
    }
}

/// Bring the interface up. Needs CAP_NET_ADMIN; without it the image's
/// own hotplug hook has to do it, and the failure is logged once.
pub fn bring_up(iface: &str) -> bool {
    // SAFETY: a datagram socket for the ioctls, closed before returning;
    // the ifreq is a zeroed C struct filled by the kernel.
    unsafe {
        let fd = libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0);
        if fd < 0 {
            return false;
        }
        let mut ifr: libc::ifreq = std::mem::zeroed();
        for (dst, src) in ifr
            .ifr_name
            .iter_mut()
            .zip(iface.bytes().take(libc::IFNAMSIZ - 1))
        {
            *dst = src as libc::c_char;
        }
        let mut ok = libc::ioctl(fd, libc::SIOCGIFFLAGS as _, &mut ifr) == 0;
        if ok && ifr.ifr_ifru.ifru_flags & libc::IFF_UP as libc::c_short == 0 {
            ifr.ifr_ifru.ifru_flags |= (libc::IFF_UP | libc::IFF_RUNNING) as libc::c_short;
            ok = libc::ioctl(fd, libc::SIOCSIFFLAGS as _, &ifr) == 0;
            if ok {
                log::info!("ncm: brought {iface} up");
            }
        }
        libc::close(fd);
        ok
    }
}

/// Claiming the usbmux interface re-enumerates the device and can knock
/// the kernel's drivers off the NCM interfaces; binding them again brings
/// the link back. A sysfs write, so it works as root only; harmless when
/// refused.
pub fn rebind(sysname: &str) {
    let _ = fs::write("/sys/bus/usb/drivers/ipheth/bind", format!("{sysname}:6.2"));
    let _ = fs::write(
        "/sys/bus/usb/drivers/cdc_ncm/bind",
        format!("{sysname}:6.3"),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_has_no_link_local_and_unknown_names_fall_back() {
        assert_eq!(link_local("lo"), None);
        assert_eq!(link_local("no-such-interface"), None);
        assert_eq!(interface_of("no-such-device"), "usb0");
    }
}
