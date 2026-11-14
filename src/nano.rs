//! Status and firmware update for Pendragon accessories (the Xbox Nano
//! Controller and relatives), which speak Microsoft's Companion Protocol over
//! HID instead of GIP. Called from `main.rs`'s `info` and `update` commands
//! once device selection finds a Pendragon accessory rather than a GIP
//! controller. Identity, status, and the full firmware-write path have been
//! checked against a physical retail Nano; QEMU covers retry and resume cases.

use crate::companion::{
    firmware::Image,
    protocol::{DeviceInfo, Status, UpdateStatus},
    updater::{DeviceInfoReply, Event, Options, Session},
};
use crate::feed;
use crate::usb::{self, NanoDevice, NanoLink};
use std::{
    fs,
    io::{self, BufRead, Write},
    path::Path,
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

/// How long to wait for the device to re-enumerate after an update; it has
/// been seen to restart itself about 1.5 s after `finish`.
const RESTART_TIMEOUT: Duration = Duration::from_secs(30);

fn version(parts: [u16; 4]) -> String {
    format!("{}.{}.{}.{}", parts[0], parts[1], parts[2], parts[3])
}

/// Not cryptographically random, just unlikely to repeat; it only needs to
/// tell this session's open request apart from a stale one.
fn session_id() -> u32 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    nanos ^ std::process::id()
}

fn open_session(link: &mut NanoLink) -> Result<Session<'_, NanoLink>, String> {
    Session::open(link, session_id(), Options::default())
}

fn print_device_info(info: &DeviceInfo) {
    println!("Hardware revision {:#06x}", info.hardware_revision);
    println!("Firmware          {}", version(info.firmware_version));
    println!(
        "Updatable         {}",
        if info.is_updatable() { "yes" } else { "no" }
    );
}

/// Older negotiated protocols fall back to a reply with no capabilities or
/// firmware version at all (see `protocol::LegacyDeviceInfo`); print what it
/// has, plus the running firmware version if the (read-only) update-status
/// opcode gave one, instead of guessing the rest.
fn print_legacy_device_info(
    info: &crate::companion::protocol::LegacyDeviceInfo,
    running_version: Option<[u16; 4]>,
) {
    println!("Hardware revision {:#06x}", info.hardware_revision);
    println!("Serial            {}", info.serial_string());
    match running_version {
        Some(firmware) => println!("Firmware          {}", version(firmware)),
        None => println!(
            "Firmware          unknown (device answered the older device-info opcode, which \
             does not report it, and the update-status opcode did not answer either)"
        ),
    }
    match info.is_updatable() {
        Some(updatable) => println!("Updatable         {}", if updatable { "yes" } else { "no" }),
        None => {
            println!("Updatable         unknown (this device's reply did not include capabilities)")
        }
    }
}

pub fn status(nano: &NanoDevice, trace: bool) -> Result<(), String> {
    let mut link = NanoLink::open(nano, trace)?;
    let mut session = open_session(&mut link)?;
    match session.device_info()? {
        DeviceInfoReply::Full(info) => print_device_info(&info),
        DeviceInfoReply::Legacy(info) => {
            // Best-effort: `update_status` is read-only (no write, no device
            // state change), so a failure here should not fail `info`
            // itself, which already has everything `device_info` could give.
            let running_version = session
                .update_status()
                .ok()
                .map(|status| status.running_version);
            print_legacy_device_info(&info, running_version);
        }
    }
    Ok(())
}

/// Send only Companion update-start (0x11), selected by exact USB serial.
/// This changes device update state but deliberately sends no firmware blocks.
#[cfg(debug_assertions)]
pub fn start_probe(
    serial: &str,
    image_path: Option<&Path>,
    latest: bool,
    trace: bool,
) -> Result<(), String> {
    let mut matches = Vec::new();
    for device in usb::find_nano_devices()? {
        let descriptor = device
            .device
            .device_descriptor()
            .map_err(|e| e.to_string())?;
        let Some(index) = descriptor.serial_number_string_index() else {
            continue;
        };
        let handle = device.device.open().map_err(|e| e.to_string())?;
        let found = handle
            .read_string_descriptor_ascii(index)
            .map_err(|e| e.to_string())?;
        if found == serial {
            matches.push(device);
        }
    }
    if matches.len() != 1 {
        return Err(format!(
            "expected exactly one connected Nano with serial {serial}, found {}",
            matches.len()
        ));
    }
    let nano = matches.remove(0);
    if nano.vendor_id != 0x045e || nano.product_id != 0x0b4e {
        return Err(format!(
            "refusing start probe for unexpected USB ID {:04x}:{:04x}",
            nano.vendor_id, nano.product_id
        ));
    }

    let mut link = NanoLink::open(&nano, trace)?;
    let mut session = open_session(&mut link)?;
    let (vendor_id, hardware_revision, running_version, updatable) = match session.device_info()? {
        DeviceInfoReply::Full(info) => (
            info.vendor_id,
            info.hardware_revision,
            info.firmware_version,
            info.is_updatable(),
        ),
        DeviceInfoReply::Legacy(info) => {
            let status = session.update_status()?;
            validate_update_status(&status)?;
            let updatable = info.is_updatable().ok_or_else(|| {
                "device did not report whether firmware updates are supported".to_owned()
            })?;
            (
                info.vendor_id,
                info.hardware_revision,
                status.running_version,
                updatable,
            )
        }
    };
    if !updatable {
        return Err("device does not report itself as updatable".to_owned());
    }
    if latest && image_path.is_some() {
        return Err("use either --image or --latest".to_owned());
    }
    let (image, _) = load_image(image_path, vendor_id, hardware_revision)?;
    let status = session.update_status()?;
    validate_update_status(&status)?;
    if image.version == running_version {
        return Err("image version equals running firmware; refusing start probe".to_owned());
    }

    println!(
        "Target {} ({serial}), firmware {} -> {}",
        nano.location(),
        version(running_version),
        image.version_string()
    );
    println!(
        "Sending update start only. This changes device state; no firmware blocks will be sent."
    );
    let reply = session.start_update(&status, &image)?;
    println!(
        "Start reply: {:?}, resume offset {:#x}, slot {}",
        reply.status, reply.resume_offset, reply.slot
    );
    if reply.status != Status::Ok {
        return Err(format!("device rejected update start: {:?}", reply.status));
    }
    drop(session);
    drop(link);

    // A separate session confirms whether the device now reports an active
    // update. This remains a read-only status request (0x03/0x14).
    let mut link = NanoLink::open(&nano, trace)?;
    let mut session = open_session(&mut link)?;
    let after = session.update_status()?;
    println!(
        "Fresh-session status: {:?}, flags {:#04x}, firmware {}, mode {}",
        after.status,
        after.flags,
        version(after.running_version),
        after.mode
    );
    println!("Probe complete; no firmware data or finish command was sent.");
    Ok(())
}

/// The image to install: a local PPAS file, or the current firmware for the
/// device's hardware revision from Microsoft's flight feed (see
/// `feed::NANO_FEED_URL`; it is not the feed the Xbox Accessories app's own
/// update check uses.
fn load_image(
    image_path: Option<&Path>,
    vendor_id: u16,
    hardware_revision: u16,
) -> Result<(Image, Option<[u16; 4]>), String> {
    let feed = feed::fetch_nano_feed()?;
    let entry = feed
        .device_by_hardware_revision(vendor_id, hardware_revision)
        .ok_or_else(|| {
            format!(
                "the flight feed has no entry for hardware revision {:#06x}",
                hardware_revision
            )
        })?;
    let file = entry
        .files
        .first()
        .ok_or_else(|| format!("the feed lists no firmware for {}", entry.id))?;
    let required_version = entry
        .required
        .first()
        .map(|required| {
            required.components().ok_or_else(|| {
                format!(
                    "invalid required firmware version {:?} in feed",
                    required.version
                )
            })
        })
        .transpose()?;
    if let Some(path) = image_path {
        let bytes = fs::read(path)
            .map_err(|error| format!("could not read {}: {error}", path.display()))?;
        return Image::parse(&bytes).map(|image| (image, required_version));
    }
    let directory = feed::cache_dir();
    let (path, image) = feed::download_image(&feed, file, &directory)?;
    println!(
        "{} ({}) firmware {} [{}], SHA-256 verified",
        entry.id,
        entry.codename,
        image.version_string(),
        file.tag
    );
    println!("{}", path.display());
    Ok((image, required_version))
}

pub fn update(
    nano: &NanoDevice,
    image_path: Option<&Path>,
    dry_run: bool,
    yes: bool,
    trace: bool,
) -> Result<(), String> {
    let mut link = NanoLink::open(nano, trace)?;
    let mut session = open_session(&mut link)?;
    let (vendor_id, hardware_revision, running_version, updatable) = match session.device_info()? {
        DeviceInfoReply::Full(info) => {
            print_device_info(&info);
            (
                info.vendor_id,
                info.hardware_revision,
                info.firmware_version,
                info.is_updatable(),
            )
        }
        DeviceInfoReply::Legacy(info) => {
            // The longer 0x23 legacy reply carries the capability bits but,
            // like 0x1c, has no firmware version. Get the running version from
            // the read-only update-status request before deciding whether to
            // continue.
            let status = session.update_status()?;
            print_legacy_device_info(&info, Some(status.running_version));
            let updatable = info.is_updatable().ok_or_else(|| {
                "the device did not report whether firmware updates are supported".to_owned()
            })?;
            validate_update_status(&status)?;
            (
                info.vendor_id,
                info.hardware_revision,
                status.running_version,
                updatable,
            )
        }
    };
    if !updatable {
        return Err("the device does not report itself as updatable".to_owned());
    }

    let (image, required_version) = load_image(image_path, vendor_id, hardware_revision)?;
    println!(
        "Image: firmware {} ({:#x} bytes)",
        image.version_string(),
        image.data.len()
    );
    if image.version == running_version {
        println!("The device already runs this firmware; nothing to do.");
        return Ok(());
    }
    let finish_flags = if required_version.is_some_and(|required| running_version < required) {
        3
    } else {
        1
    };
    session.set_finish_flags(finish_flags);
    println!("Update finish flags: {finish_flags:#04x}");
    if dry_run {
        println!(
            "Would update {} -> {}",
            version(running_version),
            image.version_string()
        );
        return Ok(());
    }
    if !yes {
        println!(
            "\nThis writes firmware to the device and will restart it. Keep it connected \
             until the update finishes; an interrupted or failed update may leave the device \
             unusable."
        );
        print!("Type 'update' to continue: ");
        io::stdout().flush().ok();
        let mut answer = String::new();
        io::stdin()
            .lock()
            .read_line(&mut answer)
            .map_err(|error| error.to_string())?;
        if answer.trim() != "update" {
            return Err("cancelled".to_owned());
        }
    }

    let mut line = crate::progress::ProgressLine::new();
    let mut report = move |event: &Event| match event {
        Event::UpToDate => line.finish("Already up to date."),
        Event::Begin {
            resume_offset,
            total,
        } => {
            line.break_line();
            println!("  writing from byte {resume_offset} of {total:#x}");
        }
        Event::Progress { done, total } => {
            let percent = u64::from(*done) * 100 / u64::from(*total).max(1);
            line.update(&format!("  writing {percent:>3}%"), percent);
        }
        Event::Finished => {
            line.finish("Finished. The device restarts itself into the new firmware.")
        }
    };
    session.update(&image, &mut report)?;
    // Release the interface before the device drops off USB and comes back
    // under a (possibly different) bus address.
    drop(link);

    println!("Waiting for the device to restart...");
    match wait_for_restart(nano) {
        Some(restarted) => match query_after_restart(&restarted, trace) {
            Ok(running_version) if running_version == image.version => println!(
                "Device is back, now running firmware {}",
                version(running_version)
            ),
            Ok(running_version) => {
                return Err(format!(
                    "device restarted on firmware {}, not {}; the update did not take",
                    version(running_version),
                    image.version_string()
                ));
            }
            Err(error) => {
                return Err(format!(
                    "device came back but its firmware could not be verified ({error}); \
                     unplug it, plug it back in and run `info`"
                ));
            }
        },
        None => {
            return Err(format!(
                "device did not come back within {} s; unplug it, plug it back in and run `info`",
                RESTART_TIMEOUT.as_secs()
            ));
        }
    }
    Ok(())
}

fn validate_update_status(status: &UpdateStatus) -> Result<(), String> {
    if !matches!(status.status, Status::Ok | Status::NotUpdating) {
        return Err(format!("device reported update status {:?}", status.status));
    }
    if status.mode == 5 {
        return Err(
            "device is waiting for update consent (mode 5); this is not implemented".to_owned(),
        );
    }
    Ok(())
}

/// Re-enumeration can briefly race the device finishing its own boot: the
/// interface may not be claimable yet, or a session opened too early may get
/// no reply. Retry for a few seconds before giving up, as GIP's own restart
/// handling (`UsbLink::reconnect`) tolerates the same transient errors.
fn query_after_restart(nano: &NanoDevice, trace: bool) -> Result<[u16; 4], String> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let attempt = (|| {
            let mut link = NanoLink::open(nano, trace)?;
            let mut session = open_session(&mut link)?;
            match session.device_info()? {
                DeviceInfoReply::Full(info) => Ok(info.firmware_version),
                DeviceInfoReply::Legacy(_) => Ok(session.update_status()?.running_version),
            }
        })();
        match attempt {
            Ok(running_version) => return Ok(running_version),
            Err(error) if Instant::now() >= deadline => return Err(error),
            Err(_) => thread::sleep(Duration::from_millis(200)),
        }
    }
}

/// Wait for a Pendragon accessory of the same model as `nano` to drop off the
/// bus and come back, up to [`RESTART_TIMEOUT`] total. The device has been
/// seen to restart itself about 1.5 s after `finish`, so it is usually still
/// there right after the update finishes; waiting for it to disappear first
/// (as GIP controllers' restart handling also does, in `UsbLink::reconnect`)
/// avoids mistaking the pre-restart device still running the old firmware
/// for the restarted one. Matched by VID/PID: this transport does not read a
/// serial number to tell two identical accessories apart.
fn wait_for_restart(nano: &NanoDevice) -> Option<NanoDevice> {
    let deadline = Instant::now() + RESTART_TIMEOUT;
    let matches = |d: &NanoDevice| d.vendor_id == nano.vendor_id && d.product_id == nano.product_id;
    let present = || {
        usb::find_nano_devices()
            .unwrap_or_default()
            .into_iter()
            .find(|d| matches(d))
    };

    while present().is_some() {
        if Instant::now() >= deadline {
            return None;
        }
        thread::sleep(Duration::from_millis(100));
    }
    loop {
        if let Some(found) = present() {
            return Some(found);
        }
        if Instant::now() >= deadline {
            return None;
        }
        thread::sleep(Duration::from_millis(100));
    }
}
