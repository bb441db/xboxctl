//! Status and firmware update for Pendragon accessories (the Xbox Nano
//! Controller and relatives), which speak Microsoft's Companion Protocol over
//! HID instead of GIP. Called from `main.rs`'s `info` and `update` commands
//! once device selection finds a Pendragon accessory rather than a GIP
//! controller. See `docs/nano.md` for what is and is not known about this
//! protocol, and its biggest caveat: none of it has been checked against a
//! physical Nano yet.

use crate::companion::{
    firmware::Image,
    protocol::DeviceInfo,
    updater::{Event, Options, Session},
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

pub fn status(nano: &NanoDevice, trace: bool) -> Result<(), String> {
    let mut link = NanoLink::open(nano, trace)?;
    let mut session = open_session(&mut link)?;
    print_device_info(&session.device_info()?);
    Ok(())
}

/// The image to install: a local PPAS file, or the current firmware for the
/// device's hardware revision from Microsoft's flight feed (see
/// `feed::NANO_FEED_URL`; it is not the feed the Xbox Accessories app's own
/// update check uses, and has not been checked against physical hardware).
fn load_image(image_path: Option<&Path>, info: &DeviceInfo) -> Result<Image, String> {
    if let Some(path) = image_path {
        let bytes = fs::read(path)
            .map_err(|error| format!("could not read {}: {error}", path.display()))?;
        return Image::parse(&bytes);
    }
    let feed = feed::fetch_nano_feed()?;
    let entry = feed
        .device_by_hardware_revision(info.vendor_id, info.hardware_revision)
        .ok_or_else(|| {
            format!(
                "the flight feed has no entry for hardware revision {:#06x}",
                info.hardware_revision
            )
        })?;
    let file = entry
        .files
        .first()
        .ok_or_else(|| format!("the feed lists no firmware for {}", entry.id))?;
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
    Ok(image)
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
    let info = session.device_info()?;
    print_device_info(&info);
    if !info.is_updatable() {
        return Err("the device does not report itself as updatable".to_owned());
    }

    let image = load_image(image_path, &info)?;
    println!(
        "Image: firmware {} ({:#x} bytes)",
        image.version_string(),
        image.data.len()
    );
    if image.version == info.firmware_version {
        println!("The device already runs this firmware; nothing to do.");
        return Ok(());
    }
    if dry_run {
        println!(
            "Would update {} -> {}",
            version(info.firmware_version),
            image.version_string()
        );
        return Ok(());
    }
    if !yes {
        println!(
            "\nThis writes firmware to the device and will restart it. Keep it connected \
             until the update finishes. This path has not been exercised against a physical \
             Nano, only an emulated one (see docs/nano.md); an interrupted or failed update \
             may leave the device unusable."
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
            Ok(info) => println!(
                "Device is back, now running firmware {}",
                version(info.firmware_version)
            ),
            Err(error) => println!(
                "The device came back, but did not answer: {error}. If it stays unresponsive, \
                 unplug it and plug it back in."
            ),
        },
        None => println!(
            "Update finished, but the device did not come back within {} s. If it stays \
             disconnected, unplug it and plug it back in.",
            RESTART_TIMEOUT.as_secs()
        ),
    }
    Ok(())
}

/// Re-enumeration can briefly race the device finishing its own boot: the
/// interface may not be claimable yet, or a session opened too early may get
/// no reply. Retry for a few seconds before giving up, as GIP's own restart
/// handling (`UsbLink::reconnect`) tolerates the same transient errors.
fn query_after_restart(nano: &NanoDevice, trace: bool) -> Result<DeviceInfo, String> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let attempt = (|| {
            let mut link = NanoLink::open(nano, trace)?;
            let mut session = open_session(&mut link)?;
            session.device_info()
        })();
        match attempt {
            Ok(info) => return Ok(info),
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
