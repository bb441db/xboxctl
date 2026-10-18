//! GIP transport over libusb: controller discovery and the interrupt
//! endpoints of interface 0.

use crate::gip::updater::Link;
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

#[cfg(target_os = "macos")]
const ACCESS_HINT: &str = "On macOS the system driver owns the controller; run with sudo.";
#[cfg(not(target_os = "macos"))]
const ACCESS_HINT: &str = "Install the udev rule from the README or run as root.";

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
            rusb::Error::Access => format!(
                "no permission to open {}. {ACCESS_HINT}",
                controller.location()
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
        let deadline = Instant::now() + timeout;
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

        // On macOS the restarted controller sometimes does not re-enumerate
        // until it is unplugged and plugged back in. Reconnecting matches it
        // by serial number, so a replug lets the update carry on.
        let dropped = Instant::now();
        let mut hinted = false;
        let mut last_error = None;
        loop {
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
                            eprintln!("reconnected at {}", controller.location());
                        }
                        *self = Self::start(&controller, handle, self.trace)?;
                        return Ok(());
                    }
                    Err(
                        error @ (rusb::Error::Access
                        | rusb::Error::Busy
                        | rusb::Error::NoDevice
                        | rusb::Error::Io),
                    ) => last_error = Some((controller.location(), error)),
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
                    Some((location, rusb::Error::Access)) => format!(
                        "no permission to reopen the restarted controller at {location}. \
                         {ACCESS_HINT}"
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

pub fn hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
}
