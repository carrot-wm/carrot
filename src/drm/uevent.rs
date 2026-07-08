// raw netlink uevent socket. the kernel broadcasts kobject events on
// group 1; drm hotplug arrives as a change event with HOTPLUG=1. we only
// learn THAT something changed - the connector re-probe sorts out what.

use rustix::net::netlink::{self, SocketAddrNetlink};
use rustix::net::{AddressFamily, SocketFlags, SocketType, bind, socket_with};
use std::os::fd::OwnedFd;

pub fn open() -> Result<OwnedFd, String> {
    let fd = socket_with(
        AddressFamily::NETLINK,
        SocketType::DGRAM,
        SocketFlags::CLOEXEC,
        Some(netlink::KOBJECT_UEVENT),
    )
    .map_err(|e| format!("netlink socket: {e}"))?;
    bind(&fd, &SocketAddrNetlink::new(0, 1)).map_err(|e| format!("netlink bind: {e}"))?;
    Ok(fd)
}

/// datagram announces a drm hotplug?
pub fn is_drm_change(buf: &[u8]) -> bool {
    let mut drm = false;
    let mut hotplug = false;
    for part in buf.split(|&b| b == 0) {
        match part {
            b"SUBSYSTEM=drm" => drm = true,
            b"HOTPLUG=1" => hotplug = true,
            _ => {}
        }
    }
    drm && hotplug
}

/// input event-node add/remove? -> (added, devname like "input/event5")
pub fn input_change(buf: &[u8]) -> Option<(bool, String)> {
    let mut input = false;
    let mut added = None;
    let mut devname = None;
    for part in buf.split(|&b| b == 0) {
        if part == b"SUBSYSTEM=input" {
            input = true;
        } else if part == b"ACTION=add" {
            added = Some(true);
        } else if part == b"ACTION=remove" {
            added = Some(false);
        } else if let Some(v) = part.strip_prefix(b"DEVNAME=") {
            devname = std::str::from_utf8(v).ok().map(str::to_owned);
        }
    }
    if !input {
        return None;
    }
    // only eventN nodes; mouseN/mice/jsN are legacy interfaces
    let name = devname?;
    if !name.rsplit('/').next()?.starts_with("event") {
        return None;
    }
    Some((added?, name))
}

/// MAJOR=/MINOR= pair as a devnum; the node itself is already gone on remove
pub fn devnum(buf: &[u8]) -> Option<u64> {
    let mut major = None;
    let mut minor = None;
    for part in buf.split(|&b| b == 0) {
        if let Some(v) = part.strip_prefix(b"MAJOR=") {
            major = std::str::from_utf8(v).ok()?.parse::<u32>().ok();
        } else if let Some(v) = part.strip_prefix(b"MINOR=") {
            minor = std::str::from_utf8(v).ok()?.parse::<u32>().ok();
        }
    }
    Some(rustix::fs::makedev(major?, minor?))
}

#[cfg(test)]
mod tests {
    use super::{devnum, input_change, is_drm_change, open};

    // group 1 (kernel broadcasts) binds without privileges
    #[test]
    fn the_netlink_socket_opens() {
        open().unwrap();
    }

    #[test]
    fn drm_hotplug_is_recognized() {
        let ev = b"change@/devices/pci0/drm/card0\0ACTION=change\0SUBSYSTEM=drm\0HOTPLUG=1\0DEVNAME=dri/card0\0";
        assert!(is_drm_change(ev));
        let other = b"add@/devices/usb1\0ACTION=add\0SUBSYSTEM=usb\0";
        assert!(!is_drm_change(other));
        let nohp = b"change@/devices/pci0/drm/card0\0SUBSYSTEM=drm\0";
        assert!(!is_drm_change(nohp));
    }

    #[test]
    fn input_hotplug_is_recognized() {
        let add = b"add@/devices/usb1/input/input12/event7\0ACTION=add\0SUBSYSTEM=input\0DEVNAME=input/event7\0MAJOR=13\0MINOR=71\0";
        assert_eq!(input_change(add), Some((true, "input/event7".to_string())));
        let rm = b"remove@/devices/usb1/input/input12/event7\0ACTION=remove\0SUBSYSTEM=input\0DEVNAME=input/event7\0MAJOR=13\0MINOR=71\0";
        assert_eq!(input_change(rm), Some((false, "input/event7".to_string())));
    }

    #[test]
    fn non_event_nodes_are_ignored() {
        // wrong subsystem
        let usb = b"add@/devices/usb1\0ACTION=add\0SUBSYSTEM=usb\0DEVNAME=bus/usb/001/002\0";
        assert_eq!(input_change(usb), None);
        // legacy mouse node
        let mouse = b"add@/devices/usb1/input/input12/mouse3\0ACTION=add\0SUBSYSTEM=input\0DEVNAME=input/mouse3\0";
        assert_eq!(input_change(mouse), None);
        // the parent inputN device carries no DEVNAME
        let parent = b"add@/devices/usb1/input/input12\0ACTION=add\0SUBSYSTEM=input\0";
        assert_eq!(input_change(parent), None);
        // bind/unbind and friends aren't add/remove
        let bind = b"bind@/devices/usb1/input/input12/event7\0ACTION=bind\0SUBSYSTEM=input\0DEVNAME=input/event7\0";
        assert_eq!(input_change(bind), None);
    }

    #[test]
    fn devnum_comes_from_major_minor() {
        let ev = b"remove@/devices/usb1/input/input12/event7\0ACTION=remove\0SUBSYSTEM=input\0DEVNAME=input/event7\0MAJOR=13\0MINOR=71\0";
        assert_eq!(devnum(ev), Some(rustix::fs::makedev(13, 71)));
        let none = b"remove@/devices/usb1/input/input12\0ACTION=remove\0SUBSYSTEM=input\0";
        assert_eq!(devnum(none), None);
    }
}
