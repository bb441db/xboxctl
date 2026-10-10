//! GIP transport over libusb. Works on Linux, macOS and Windows; on Windows the
//! controller must be bound to WinUSB (for example with Zadig) instead of
//! xboxgip.sys.

use gip::updater::Link;
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

/// Interrupt endpoints of interface 0, with a reader thread that keeps an
/// IN transfer pending.
pub struct UsbLink {
    handle: Arc<Handle>,
    packets: mpsc::Receiver<Result<Vec<u8>, String>>,
    stop: Arc<AtomicBool>,
    reader: Option<thread::JoinHandle<()>>,
    trace: bool,
}

impl UsbLink {
    /// Wait up to `timeout` for the device to appear, then claim it.
    pub fn open(vendor_id: u16, product_id: u16, timeout: Duration, trace: bool) -> Result<Self, String> {
        let deadline = Instant::now() + timeout;
        let handle = loop {
            if let Some(handle) = rusb::open_device_with_vid_pid(vendor_id, product_id) {
                // Detach xpad/xone on Linux; unsupported (and unnecessary)
                // elsewhere.
                let _ = handle.set_auto_detach_kernel_driver(true);
                match handle.claim_interface(INTERFACE) {
                    Ok(()) => break handle,
                    Err(rusb::Error::NoDevice) => {}
                    Err(error) => {
                        return Err(format!(
                            "could not claim interface {INTERFACE} of {vendor_id:04x}:{product_id:04x}: {error}. \
                             Check USB permissions; on Windows bind the device to WinUSB."
                        ));
                    }
                }
            }
            if Instant::now() >= deadline {
                return Err(format!("no {vendor_id:04x}:{product_id:04x} device found"));
            }
            thread::sleep(Duration::from_millis(200));
        };

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
        Ok(Self { handle, packets, stop, reader: Some(reader), trace })
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
        self.stop.store(true, Ordering::Relaxed);
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
        let _ = self.handle.release_interface(INTERFACE);
    }
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect::<Vec<_>>().join(" ")
}
