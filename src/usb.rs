//! GIP transport over libusb: controller discovery and the interrupt
//! endpoints of interface 0.

use crate::{gip::updater::Link, holders};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

const INTERFACE: u8 = 0;
const ENDPOINT_IN: u8 = 0x82;
const ENDPOINT_OUT: u8 = 0x02;

type Handle = rusb::DeviceHandle<rusb::GlobalContext>;

/// GIP data interface: vendor class 0xff, subclass 0x47, protocol 0xd0.
const GIP_INTERFACE_CLASS: (u8, u8, u8) = (0xff, 0x47, 0xd0);
/// How long a restarting controller may stay away before asking for a replug.
const REPLUG_HINT_AFTER: Duration = Duration::from_secs(15);

/// A GIP controller found on the USB bus.
pub struct Controller {
    pub device: rusb::Device<rusb::GlobalContext>,
    pub bus: u8,
    pub address: u8,
    pub vendor_id: u16,
    pub product_id: u16,
    pub bcd_device: u16,
}

impl Controller {
    /// Model name and the Xbox Accessories app's codename, where known.
    pub fn model(&self) -> (&'static str, Option<&'static str>) {
        match (self.vendor_id, self.product_id) {
            (0x045e, 0x02d1) => ("Xbox One Controller", Some("ARGO")),
            (0x045e, 0x02dd) => ("Xbox One Controller (2015)", Some("LANG")),
            (0x045e, 0x02e3) => ("Xbox One Elite Controller", Some("TROY")),
            (0x045e, 0x02ea) => ("Xbox One S Controller", Some("CRET")),
            (0x045e, 0x0b00) => ("Xbox Elite Controller Series 2", Some("DELP")),
            (0x045e, 0x0b0a) => ("Xbox Adaptive Controller", Some("ZEPH")),
            (0x045e, 0x0b1a | 0x0b1c) => ("Xbox Adaptive Joystick", Some("DURA")),
            (0x045e, 0x0b12) => ("Xbox Series X|S Controller", Some("MERL")),
            _ => ("GIP controller", None),
        }
    }

    /// Firmware major.minor as reported in bcdDevice (5.23 is 0x0517).
    pub fn firmware_major_minor(&self) -> (u8, u8) {
        ((self.bcd_device >> 8) as u8, self.bcd_device as u8)
    }

    /// USB serial string, if the device can be opened.
    pub fn serial(&self) -> Option<String> {
        let handle = self.device.open().ok()?;
        let index = self
            .device
            .device_descriptor()
            .ok()?
            .serial_number_string_index()?;
        handle.read_string_descriptor_ascii(index).ok()
    }

    pub fn location(&self) -> String {
        format!("{:03}:{:03}", self.bus, self.address)
    }
}

fn is_gip(device: &rusb::Device<rusb::GlobalContext>) -> bool {
    let Ok(config) = device.config_descriptor(0) else {
        return false;
    };
    config
        .interfaces()
        .flat_map(|interface| interface.descriptors())
        .any(|descriptor| {
            (
                descriptor.class_code(),
                descriptor.sub_class_code(),
                descriptor.protocol_code(),
            ) == GIP_INTERFACE_CLASS
        })
}

/// Every GIP controller on the bus. Reads descriptors only; nothing is opened.
pub fn find_controllers() -> Result<Vec<Controller>, String> {
    let devices =
        rusb::devices().map_err(|error| format!("could not list USB devices: {error}"))?;
    Ok(devices
        .iter()
        .filter(is_gip)
        .filter_map(|device| {
            let descriptor = device.device_descriptor().ok()?;
            let version = descriptor.device_version();
            Some(Controller {
                bus: device.bus_number(),
                address: device.address(),
                vendor_id: descriptor.vendor_id(),
                product_id: descriptor.product_id(),
                // rusb decodes bcdDevice as BCD (major is two decimal digits);
                // re-encode it to get the raw value back.
                bcd_device: u16::from(version.major() / 10) << 12
                    | u16::from(version.major() % 10) << 8
                    | u16::from(version.minor()) << 4
                    | u16::from(version.sub_minor()),
                device,
            })
        })
        .collect())
}

/// Every Microsoft device on the bus, GIP or not, for tracing a restart that
/// might come back under another product ID. Reads descriptors only.
fn microsoft_devices() -> String {
    let Ok(devices) = rusb::devices() else {
        return String::new();
    };
    devices
        .iter()
        .filter_map(|device| {
            let descriptor = device.device_descriptor().ok()?;
            (descriptor.vendor_id() == 0x045e).then(|| {
                format!(
                    "{:03}:{:03} {:04x}:{:04x}{}",
                    device.bus_number(),
                    device.address(),
                    descriptor.vendor_id(),
                    descriptor.product_id(),
                    if is_gip(&device) { " (GIP)" } else { "" }
                )
            })
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Pick a controller by `BUS:ADDRESS`, or the only one connected.
pub fn select_controller(location: Option<&str>) -> Result<Controller, String> {
    let mut controllers = find_controllers()?;
    match location {
        Some(location) => {
            let (bus, address) = location
                .split_once(':')
                .and_then(|(bus, address)| {
                    Some((bus.parse::<u8>().ok()?, address.parse::<u8>().ok()?))
                })
                .ok_or_else(|| {
                    format!("invalid device {location:?}; use BUS:ADDRESS from `xboxctl list`")
                })?;
            controllers
                .into_iter()
                .find(|c| c.bus == bus && c.address == address)
                .ok_or_else(|| format!("no GIP controller at {location}"))
        }
        None => match controllers.len() {
            0 => Err("no Xbox controller found".to_owned()),
            1 => Ok(controllers.remove(0)),
            _ => Err(
                "several controllers are connected; choose one with --device BUS:ADDRESS"
                    .to_owned(),
            ),
        },
    }
}

fn serial_number(controller: &Controller, handle: &Handle) -> Option<String> {
    let index = controller
        .device
        .device_descriptor()
        .ok()?
        .serial_number_string_index()?;
    handle.read_string_descriptor_ascii(index).ok()
}

/// Other programs that have this controller open; empty when none is found.
pub fn holders_of(controller: &Controller) -> Vec<holders::Holder> {
    holders::find(
        controller.vendor_id,
        controller.product_id,
        controller.bus,
        controller.address,
    )
}

/// Which other programs have this model of controller open, as a line to
/// append to an error; empty when none is found. Looks the controller up again,
/// because it may have restarted at another address.
pub fn holder_hint(vendor_id: u16, product_id: u16) -> String {
    let holders: Vec<_> = find_controllers()
        .unwrap_or_default()
        .iter()
        .filter(|c| (c.vendor_id, c.product_id) == (vendor_id, product_id))
        .flat_map(holders_of)
        .collect();
    if holders.is_empty() {
        return String::new();
    }
    let quit = if holders.len() == 1 { "it" } else { "them" };
    format!("\n{}; quit {quit}.", holders::sentence(&holders))
}

/// The error for a controller that refused to open for lack of access.
/// `subject` is what could not be opened. Names the programs holding the
/// controller if we can find them; otherwise gives the usual cause on this
/// platform.
fn access_error(subject: &str, vendor_id: u16, product_id: u16) -> String {
    let holders = holder_hint(vendor_id, product_id);
    if !holders.is_empty() {
        return format!(
            "could not open {subject}: another program has it open.{holders}\n\
             Then unplug the controller, plug it back in and run the command again."
        );
    }
    #[cfg(target_os = "macos")]
    if crate::sys::is_root() {
        return format!(
            "could not open {subject}. Another program probably has the controller open; \
             unplug it, plug it back in and try again."
        );
    }
    #[cfg(target_os = "macos")]
    return format!(
        "no permission to open {subject}. On macOS the system driver owns the controller; \
         run with sudo."
    );
    #[cfg(not(target_os = "macos"))]
    format!(
        "no permission to open {subject}. Install the udev rule from the README or run as root."
    )
}

/// Open the controller and claim interface 0, detaching `xpad` (it is
/// reattached when the interface is released).
fn claim(controller: &Controller) -> Result<Handle, rusb::Error> {
    let handle = controller.device.open()?;
    let _ = handle.set_auto_detach_kernel_driver(true);
    handle.claim_interface(INTERFACE)?;
    Ok(handle)
}

/// Interrupt endpoints of interface 0, with a reader thread that keeps an
/// IN transfer pending.
pub struct UsbLink {
    ids: (u16, u16),
    /// USB serial number; identifies the controller across restarts.
    serial: Option<String>,
    handle: Arc<Handle>,
    packets: mpsc::Receiver<Result<Vec<u8>, String>>,
    stop: Arc<AtomicBool>,
    reader: Option<thread::JoinHandle<()>>,
    trace: bool,
}

impl UsbLink {
    pub fn open(controller: &Controller, trace: bool) -> Result<Self, String> {
        let handle = claim(controller).map_err(|error| match error {
            rusb::Error::Access => access_error(
                &controller.location(),
                controller.vendor_id,
                controller.product_id,
            ),
            rusb::Error::Busy => format!(
                "{} is in use by another program.{}",
                controller.location(),
                holder_hint(controller.vendor_id, controller.product_id)
            ),
            error => format!("could not open {}: {error}", controller.location()),
        })?;
        Self::start(controller, handle, trace)
    }

    fn start(controller: &Controller, handle: Handle, trace: bool) -> Result<Self, String> {
        let handle = Arc::new(handle);
        let stop = Arc::new(AtomicBool::new(false));
        let (sender, packets) = mpsc::channel();
        let reader = {
            let handle = Arc::clone(&handle);
            let stop = Arc::clone(&stop);
            thread::Builder::new()
                .name("gip-in".to_owned())
                .spawn(move || {
                    let mut buffer = [0u8; 64];
                    while !stop.load(Ordering::Relaxed) {
                        let result = match handle.read_interrupt(
                            ENDPOINT_IN,
                            &mut buffer,
                            Duration::from_millis(100),
                        ) {
                            Ok(length) => Ok(buffer[..length].to_vec()),
                            Err(rusb::Error::Timeout) => continue,
                            Err(rusb::Error::NoDevice) => Err("device disconnected".to_owned()),
                            Err(error) => Err(format!("USB read failed: {error}")),
                        };
                        let failed = result.is_err();
                        if sender.send(result).is_err() || failed {
                            return;
                        }
                    }
                })
                .map_err(|error| format!("could not start USB reader: {error}"))?
        };
        // Give libusb time to submit the first IN transfer; a Hello can
        // follow the first OUT message within milliseconds.
        thread::sleep(Duration::from_millis(20));
        Ok(Self {
            ids: (controller.vendor_id, controller.product_id),
            serial: serial_number(controller, &handle),
            handle,
            packets,
            stop,
            reader: Some(reader),
            trace,
        })
    }

    fn close(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
        let _ = self.handle.release_interface(INTERFACE);
    }
}

impl Link for UsbLink {
    fn send(&mut self, packet: &[u8]) -> Result<(), String> {
        let written = self
            .handle
            .write_interrupt(ENDPOINT_OUT, packet, Duration::from_secs(1))
            .map_err(|error| format!("USB write failed: {error}"))?;
        if written != packet.len() {
            return Err(format!("short USB write: {written}/{} bytes", packet.len()));
        }
        if self.trace {
            eprintln!("OUT {}", hex(packet));
        }
        Ok(())
    }

    /// Reopen the controller after it restarted and re-enumerated, with an IN
    /// transfer pending for its Hello.
    ///
    /// The restarted controller is recognised by its serial number rather
    /// than its bus address, which need not change (e.g. on macOS). First
    /// wait until this connection has dropped: after a reset the old device
    /// lingers for 1-3.6 s, and opening it then fails.
    fn reconnect(&mut self, timeout: Duration) -> Result<(), String> {
        let started = Instant::now();
        let deadline = started + timeout;
        let ms = || started.elapsed().as_millis();
        loop {
            match self
                .packets
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            {
                // Still connected; drop whatever the old device sends.
                Ok(Ok(_)) => {}
                // A read error, or the reader already stopped after one.
                Ok(Err(_)) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    return Err("the controller did not disconnect for its restart".to_owned());
                }
            }
        }
        self.close();
        if self.trace {
            eprintln!("reconnect +{} ms: disconnected", ms());
        }

        // On macOS the restarted controller sometimes does not re-enumerate
        // until it is unplugged and plugged back in. Reconnecting matches it
        // by serial number, so a replug lets the update carry on.
        let dropped = Instant::now();
        let mut hinted = false;
        let mut last_error = None;
        let mut seen = None;
        loop {
            if self.trace {
                let devices = microsoft_devices();
                if seen.as_ref() != Some(&devices) {
                    eprintln!("reconnect +{} ms: devices [{devices}]", ms());
                    seen = Some(devices);
                }
            }
            for controller in find_controllers()?
                .into_iter()
                .filter(|c| (c.vendor_id, c.product_id) == self.ids)
            {
                // Right after re-enumeration udev may not have applied the
                // uaccess permission yet, and xpad may still be binding.
                match claim(&controller) {
                    Ok(handle) => {
                        let serial = serial_number(&controller, &handle);
                        if self.serial.is_some() && serial.is_some() && serial != self.serial {
                            // A different controller of the same model.
                            let _ = handle.release_interface(INTERFACE);
                            continue;
                        }
                        if self.trace {
                            eprintln!(
                                "reconnect +{} ms: reconnected at {}",
                                ms(),
                                controller.location()
                            );
                        }
                        *self = Self::start(&controller, handle, self.trace)?;
                        return Ok(());
                    }
                    Err(
                        error @ (rusb::Error::Access
                        | rusb::Error::Busy
                        | rusb::Error::NoDevice
                        | rusb::Error::Io),
                    ) => {
                        if self.trace && last_error.as_ref().map(|(_, e)| e) != Some(&error) {
                            eprintln!(
                                "reconnect +{} ms: claiming {} failed: {error}",
                                ms(),
                                controller.location()
                            );
                        }
                        last_error = Some((controller.location(), error));
                    }
                    Err(error) => {
                        return Err(format!(
                            "could not reopen {}: {error}",
                            controller.location()
                        ));
                    }
                }
            }
            if !hinted && dropped.elapsed() >= REPLUG_HINT_AFTER {
                println!(
                    "  the controller has not come back yet; unplug it and plug it back in \
                     to continue"
                );
                hinted = true;
            }
            if Instant::now() >= deadline {
                return Err(match last_error {
                    Some((location, rusb::Error::Access)) => access_error(
                        &format!("the restarted controller at {location}"),
                        self.ids.0,
                        self.ids.1,
                    ),
                    Some((location, rusb::Error::Busy)) => format!(
                        "the restarted controller at {location} is in use by another program.{}",
                        holder_hint(self.ids.0, self.ids.1)
                    ),
                    Some((location, error)) => {
                        format!("could not reopen the restarted controller at {location}: {error}")
                    }
                    None => "the controller did not come back after its restart; unplug and \
                             replug it, then run the update again"
                        .to_owned(),
                });
            }
            thread::sleep(Duration::from_millis(50));
        }
    }

    fn recv(&mut self, timeout: Duration) -> Result<Option<Vec<u8>>, String> {
        match self.packets.recv_timeout(timeout) {
            Ok(Ok(packet)) => {
                if self.trace {
                    eprintln!("IN  {}", hex(&packet));
                }
                Ok(Some(packet))
            }
            Ok(Err(error)) => Err(error),
            Err(mpsc::RecvTimeoutError::Timeout) => Ok(None),
            Err(mpsc::RecvTimeoutError::Disconnected) => Err("USB reader stopped".to_owned()),
        }
    }
}

impl Drop for UsbLink {
    fn drop(&mut self) {
        self.close();
    }
}

/// Pendragon family: accessories that speak the Companion Protocol over HID
/// instead of GIP. Matched by VID/PID rather than the HID usage page the SDK
/// uses (`0xff45`/`0x63`), since reading the HID report descriptor needs an
/// extra control transfer this does not implement.
const PENDRAGON_PRODUCT_IDS: [u16; 5] = [0x0b1d, 0x0b45, 0x0b46, 0x0b4c, 0x0b4e];

/// A Pendragon accessory found on the USB bus.
pub struct NanoDevice {
    pub device: rusb::Device<rusb::GlobalContext>,
    pub bus: u8,
    pub address: u8,
    pub vendor_id: u16,
    pub product_id: u16,
    interface: u8,
    endpoint_in: Endpoint,
    endpoint_out: Endpoint,
}

impl NanoDevice {
    pub fn model(&self) -> &'static str {
        match self.product_id {
            0x0b4c | 0x0b4e => "Xbox Nano Controller",
            0x0b1d => "Pendragon Device B accessory",
            0x0b46 => "Pendragon Device D accessory",
            0x0b45 => "Pendragon Device E accessory",
            _ => "Pendragon accessory",
        }
    }

    pub fn location(&self) -> String {
        format!("{:03}:{:03}", self.bus, self.address)
    }
}

/// A USB endpoint address and the report size (`wMaxPacketSize`) it carries.
type Endpoint = (u8, usize);

/// The device's single HID interface and its interrupt endpoints, if it has
/// one; Pendragon accessories put every top-level collection (gamepad,
/// companion, ...) on one interface, selected by report ID.
fn hid_endpoints(device: &rusb::Device<rusb::GlobalContext>) -> Option<(u8, Endpoint, Endpoint)> {
    let config = device.config_descriptor(0).ok()?;
    for interface in config.interfaces() {
        for descriptor in interface.descriptors() {
            if descriptor.class_code() != 0x03 {
                continue;
            }
            let mut endpoint_in = None;
            let mut endpoint_out = None;
            for endpoint in descriptor.endpoint_descriptors() {
                if endpoint.transfer_type() != rusb::TransferType::Interrupt {
                    continue;
                }
                let entry = (endpoint.address(), endpoint.max_packet_size() as usize);
                match endpoint.direction() {
                    rusb::Direction::In => endpoint_in = Some(entry),
                    rusb::Direction::Out => endpoint_out = Some(entry),
                }
            }
            if let (Some(endpoint_in), Some(endpoint_out)) = (endpoint_in, endpoint_out) {
                return Some((descriptor.interface_number(), endpoint_in, endpoint_out));
            }
        }
    }
    None
}

/// Every Pendragon accessory on the bus. Reads descriptors only; nothing is
/// opened.
pub fn find_nano_devices() -> Result<Vec<NanoDevice>, String> {
    let devices =
        rusb::devices().map_err(|error| format!("could not list USB devices: {error}"))?;
    Ok(devices
        .iter()
        .filter_map(|device| {
            let descriptor = device.device_descriptor().ok()?;
            if descriptor.vendor_id() != 0x045e
                || !PENDRAGON_PRODUCT_IDS.contains(&descriptor.product_id())
            {
                return None;
            }
            let (interface, endpoint_in, endpoint_out) = hid_endpoints(&device)?;
            Some(NanoDevice {
                bus: device.bus_number(),
                address: device.address(),
                vendor_id: descriptor.vendor_id(),
                product_id: descriptor.product_id(),
                interface,
                endpoint_in,
                endpoint_out,
                device,
            })
        })
        .collect())
}

fn parse_location(location: &str) -> Option<(u8, u8)> {
    location
        .split_once(':')
        .and_then(|(bus, address)| Some((bus.parse::<u8>().ok()?, address.parse::<u8>().ok()?)))
}

/// Either kind of Xbox device on the USB bus: a GIP controller, or a
/// Pendragon accessory speaking the Companion Protocol.
pub enum AnyDevice {
    Gip(Controller),
    Nano(NanoDevice),
}

/// Every GIP controller and Pendragon accessory on the bus.
pub fn find_devices() -> Result<Vec<AnyDevice>, String> {
    let mut devices: Vec<AnyDevice> = find_controllers()?
        .into_iter()
        .map(AnyDevice::Gip)
        .collect();
    devices.extend(find_nano_devices()?.into_iter().map(AnyDevice::Nano));
    Ok(devices)
}

/// Pick a device of either kind by `BUS:ADDRESS`, or the only one connected.
/// `xboxctl info` and `xboxctl update` use this so they work on both; other
/// commands only support GIP controllers and use [`select_controller`]
/// directly, guarded by [`reject_nano`].
pub fn select_device(location: Option<&str>) -> Result<AnyDevice, String> {
    let mut devices = find_devices()?;
    match location {
        Some(location) => {
            let (bus, address) = parse_location(location).ok_or_else(|| {
                format!("invalid device {location:?}; use BUS:ADDRESS from `xboxctl list`")
            })?;
            devices
                .into_iter()
                .find(|d| match d {
                    AnyDevice::Gip(c) => c.bus == bus && c.address == address,
                    AnyDevice::Nano(n) => n.bus == bus && n.address == address,
                })
                .ok_or_else(|| format!("no device at {location}"))
        }
        None => match devices.len() {
            0 => Err("no Xbox controller or accessory found".to_owned()),
            1 => Ok(devices.remove(0)),
            _ => Err(
                "several devices are connected; choose one with --device BUS:ADDRESS".to_owned(),
            ),
        },
    }
}

/// For commands that only support a GIP controller: if `location` names a
/// Pendragon accessory, or none is named and the only connected device is
/// one, say the command does not support it instead of letting the GIP-only
/// lookup report "no controller found" as if nothing were plugged in at all.
pub fn reject_nano(location: Option<&str>, command: &str) -> Result<(), String> {
    let nanos = find_nano_devices().unwrap_or_default();
    let target = match location {
        Some(location) => {
            let Some((bus, address)) = parse_location(location) else {
                return Ok(()); // an invalid location; let the GIP lookup say so
            };
            nanos
                .into_iter()
                .find(|n| n.bus == bus && n.address == address)
        }
        None => {
            if nanos.len() == 1 && find_controllers().unwrap_or_default().is_empty() {
                nanos.into_iter().next()
            } else {
                None
            }
        }
    };
    match target {
        Some(nano) => Err(format!(
            "{} is {}; `xboxctl {command}` does not support it (no GIP interface). \
             Try `xboxctl info` or `xboxctl update` instead.",
            nano.location(),
            nano.model()
        )),
        None => Ok(()),
    }
}

/// Interrupt endpoints of a Pendragon accessory's HID interface, framed as
/// `0x0c` reports, with a reader thread that keeps an IN transfer pending.
pub struct NanoLink {
    interface: u8,
    endpoint_in: Endpoint,
    endpoint_out: Endpoint,
    handle: Arc<Handle>,
    packets: mpsc::Receiver<Result<Vec<u8>, String>>,
    stop: Arc<AtomicBool>,
    reader: Option<thread::JoinHandle<()>>,
    trace: bool,
}

impl NanoLink {
    pub fn open(device: &NanoDevice, trace: bool) -> Result<Self, String> {
        let handle = device.device.open().map_err(|error| match error {
            rusb::Error::Access => {
                access_error(&device.location(), device.vendor_id, device.product_id)
            }
            rusb::Error::Busy => format!(
                "{} is in use by another program.{}",
                device.location(),
                holder_hint(device.vendor_id, device.product_id)
            ),
            error => format!("could not open {}: {error}", device.location()),
        })?;
        let _ = handle.set_auto_detach_kernel_driver(true);
        handle
            .claim_interface(device.interface)
            .map_err(|error| format!("could not claim the companion interface: {error}"))?;

        let handle = Arc::new(handle);
        let stop = Arc::new(AtomicBool::new(false));
        let (sender, packets) = mpsc::channel();
        let endpoint_in = device.endpoint_in;
        let reader = {
            let handle = Arc::clone(&handle);
            let stop = Arc::clone(&stop);
            thread::Builder::new()
                .name("nano-in".to_owned())
                .spawn(move || {
                    let mut buffer = vec![0u8; endpoint_in.1];
                    while !stop.load(Ordering::Relaxed) {
                        let result = match handle.read_interrupt(
                            endpoint_in.0,
                            &mut buffer,
                            Duration::from_millis(100),
                        ) {
                            // Only a full report starting with the companion
                            // report ID is a message; anything else is
                            // dropped, matching the SDK's own receive check.
                            Ok(length)
                                if length == endpoint_in.1
                                    && buffer[0] == crate::companion::frame::REPORT_ID =>
                            {
                                Ok(buffer[1..length].to_vec())
                            }
                            Ok(_) => continue,
                            Err(rusb::Error::Timeout) => continue,
                            Err(rusb::Error::NoDevice) => Err("device disconnected".to_owned()),
                            Err(error) => Err(format!("USB read failed: {error}")),
                        };
                        let failed = result.is_err();
                        if sender.send(result).is_err() || failed {
                            return;
                        }
                    }
                })
                .map_err(|error| format!("could not start USB reader: {error}"))?
        };
        thread::sleep(Duration::from_millis(20));
        Ok(Self {
            interface: device.interface,
            endpoint_in,
            endpoint_out: device.endpoint_out,
            handle,
            packets,
            stop,
            reader: Some(reader),
            trace,
        })
    }

    fn close(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
        let _ = self.handle.release_interface(self.interface);
    }
}

impl crate::companion::updater::Link for NanoLink {
    fn send(&mut self, payload: &[u8]) -> Result<(), String> {
        if payload.len() + 1 > self.endpoint_out.1 {
            return Err(format!(
                "message of {} bytes does not fit the {}-byte output report",
                payload.len(),
                self.endpoint_out.1
            ));
        }
        let mut report = vec![0u8; self.endpoint_out.1];
        report[0] = crate::companion::frame::REPORT_ID;
        report[1..1 + payload.len()].copy_from_slice(payload);
        let written = self
            .handle
            .write_interrupt(self.endpoint_out.0, &report, Duration::from_secs(1))
            .map_err(|error| format!("USB write failed: {error}"))?;
        if written != report.len() {
            return Err(format!("short USB write: {written}/{} bytes", report.len()));
        }
        if self.trace {
            eprintln!("OUT {}", hex(&report));
        }
        Ok(())
    }

    fn recv(&mut self, timeout: Duration) -> Result<Option<Vec<u8>>, String> {
        match self.packets.recv_timeout(timeout) {
            Ok(Ok(payload)) => {
                if self.trace {
                    eprintln!("IN  {}", hex(&payload));
                }
                Ok(Some(payload))
            }
            Ok(Err(error)) => Err(error),
            Err(mpsc::RecvTimeoutError::Timeout) => Ok(None),
            Err(mpsc::RecvTimeoutError::Disconnected) => Err("USB reader stopped".to_owned()),
        }
    }

    fn max_fragment_data(&self) -> usize {
        self.endpoint_in
            .1
            .min(self.endpoint_out.1)
            .saturating_sub(1 + 5)
    }
}

impl Drop for NanoLink {
    fn drop(&mut self) {
        self.close();
    }
}

pub fn hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
}
