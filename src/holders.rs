//! Find the other processes that have a controller open.
//!
//! A controller that another program holds stops answering us (or refuses to
//! be claimed), and a bare "timed out" does not tell the user what to quit.
//! The lookup is best effort: it can miss processes it is not allowed to see,
//! and a failure just means no hint.

#[cfg(any(target_os = "linux", test))]
use std::path::Path;

/// A process that has the controller open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Holder {
    pub pid: u32,
    pub name: String,
}

/// Processes other than this one that hold the USB device with this
/// `bus`/`address`. `vendor_id` and `product_id` identify it where the OS
/// does not number devices the way libusb does.
pub fn find(vendor_id: u16, product_id: u16, bus: u8, address: u8) -> Vec<Holder> {
    let mut found = platform::find(vendor_id, product_id, bus, address);
    let own = std::process::id();
    found.retain(|holder| holder.pid != own);
    found.sort_by_key(|holder| holder.pid);
    found.dedup_by_key(|holder| holder.pid);
    found
}

/// `Google Chrome (pid 123), Preview (pid 456)`.
pub fn list(holders: &[Holder]) -> String {
    holders
        .iter()
        .map(|holder| format!("{} (pid {})", holder.name, holder.pid))
        .collect::<Vec<_>>()
        .join(", ")
}

/// `Google Chrome (pid 123) has the controller open`, or `... have ...` for
/// several holders.
pub fn sentence(holders: &[Holder]) -> String {
    let verb = if holders.len() == 1 { "has" } else { "have" };
    format!("{} {verb} the controller open", list(holders))
}

/// Split an IOKit creator string such as `pid 86018, Google Chrome`. Owners
/// without a pid (the system driver's `XboxUSBDevice`) are not processes we
/// could ask the user to quit.
#[cfg(any(target_os = "macos", test))]
fn parse_creator(creator: &str) -> Option<Holder> {
    let (pid, name) = creator.strip_prefix("pid ")?.split_once(',')?;
    Some(Holder {
        pid: pid.trim().parse().ok()?,
        name: name.trim().to_owned(),
    })
}

/// Processes under `proc_root` (normally `/proc`) with an open file
/// descriptor on the usbfs `node`. Processes whose descriptors we may not read
/// (other users', when not root) are skipped.
#[cfg(any(target_os = "linux", test))]
fn scan_proc(proc_root: &Path, node: &Path) -> Vec<Holder> {
    use std::fs;

    let Ok(processes) = fs::read_dir(proc_root) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for process in processes.flatten() {
        let Some(pid) = process.file_name().to_str().and_then(|n| n.parse().ok()) else {
            continue;
        };
        let Ok(descriptors) = fs::read_dir(process.path().join("fd")) else {
            continue;
        };
        let holds = descriptors
            .flatten()
            .any(|fd| fs::read_link(fd.path()).is_ok_and(|target| target == node));
        if holds {
            let name = fs::read_to_string(process.path().join("comm"))
                .map(|name| name.trim().to_owned())
                .unwrap_or_else(|_| "unknown process".to_owned());
            found.push(Holder { pid, name });
        }
    }
    found
}

#[cfg(target_os = "linux")]
mod platform {
    use super::*;

    /// Another process using the device through usbfs (libusb, WebUSB) is what
    /// makes claiming it fail. Readers of the evdev and hidraw nodes do not
    /// block us, because `xpad` is detached while we run.
    pub fn find(_vendor_id: u16, _product_id: u16, bus: u8, address: u8) -> Vec<Holder> {
        let node = format!("/dev/bus/usb/{bus:03}/{address:03}");
        scan_proc(Path::new("/proc"), Path::new(&node))
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use super::*;
    use crate::iokit::Entry;

    /// The USB device with this VID, PID and address, if the registry has it.
    fn usb_device(vendor_id: u16, product_id: u16, address: u8) -> Option<Entry> {
        Entry::services_of_class(c"IOUSBHostDevice")
            .into_iter()
            .find(|device| {
                device.number("idVendor") == Some(i64::from(vendor_id))
                    && device.number("idProduct") == Some(i64::from(product_id))
                    && device
                        .number("USB Address")
                        .or_else(|| device.number("kUSBAddress"))
                        == Some(i64::from(address))
            })
    }

    /// Whatever has the device or one of its interfaces open: the exclusive
    /// owner the host stack records, and the USB user clients attached below
    /// the device, which name their creating process.
    fn collect(entry: &Entry, depth: u32, found: &mut Vec<Holder>) {
        if let Some(owner) = entry.string("UsbExclusiveOwner") {
            found.extend(parse_creator(&owner));
        }
        let class = entry.class_name();
        if class.contains("USB")
            && class.contains("UserClient")
            && let Some(creator) = entry.string("IOUserClientCreator")
        {
            found.extend(parse_creator(&creator));
        }
        if depth > 0 {
            for child in entry.children() {
                collect(&child, depth - 1, found);
            }
        }
    }

    pub fn find(vendor_id: u16, product_id: u16, _bus: u8, address: u8) -> Vec<Holder> {
        let mut found = Vec::new();
        if let Some(device) = usb_device(vendor_id, product_id, address) {
            collect(&device, 4, &mut found);
        }
        found
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
mod platform {
    use super::*;

    pub fn find(_vendor_id: u16, _product_id: u16, _bus: u8, _address: u8) -> Vec<Holder> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, path::PathBuf};

    #[test]
    fn parses_creators() {
        assert_eq!(
            parse_creator("pid 86018, Google Chrome"),
            Some(Holder {
                pid: 86018,
                name: "Google Chrome".to_owned()
            })
        );
        assert_eq!(parse_creator("XboxUSBDevice"), None);
        assert_eq!(parse_creator("pid x, Broken"), None);
    }

    #[test]
    fn words_a_sentence() {
        let chrome = Holder {
            pid: 1,
            name: "Chrome".to_owned(),
        };
        let edge = Holder {
            pid: 2,
            name: "Edge".to_owned(),
        };
        assert_eq!(
            sentence(std::slice::from_ref(&chrome)),
            "Chrome (pid 1) has the controller open"
        );
        assert_eq!(
            sentence(&[chrome, edge]),
            "Chrome (pid 1), Edge (pid 2) have the controller open"
        );
    }

    #[test]
    fn lists_holders() {
        let holders = [
            Holder {
                pid: 1,
                name: "a".to_owned(),
            },
            Holder {
                pid: 2,
                name: "b c".to_owned(),
            },
        ];
        assert_eq!(list(&holders), "a (pid 1), b c (pid 2)");
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("xboxctl-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn scans_proc_for_a_usbfs_node() {
        let root = scratch("proc");
        let node = Path::new("/dev/bus/usb/001/004");
        // pid 10 holds the node, pid 11 another device, "self" is not a pid.
        for (pid, target, comm) in [
            ("10", node, "chrome\n"),
            ("11", Path::new("/dev/bus/usb/001/005"), "other\n"),
        ] {
            let fds = root.join(pid).join("fd");
            fs::create_dir_all(&fds).unwrap();
            std::os::unix::fs::symlink(target, fds.join("3")).unwrap();
            fs::write(root.join(pid).join("comm"), comm).unwrap();
        }
        fs::create_dir_all(root.join("self")).unwrap();

        assert_eq!(
            scan_proc(&root, node),
            vec![Holder {
                pid: 10,
                name: "chrome".to_owned()
            }]
        );
        fs::remove_dir_all(&root).unwrap();
    }
}
