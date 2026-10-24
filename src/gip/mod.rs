//! Xbox Gaming Input Protocol (GIP) framing and the UFU firmware update
//! protocol.
//!
//! The UFU protocol was reverse engineered from Windows and verified against
//! the QEMU-emulated controller and a physical Series X|S controller.

pub mod calibration;
/// Device side of UFU (simulated flash), used as the controller in tests.
/// The emulator for manual testing is the QEMU `usb-xbox-gip` device.
#[cfg(test)]
pub mod device;
pub mod frame;
pub mod rdhm;
pub mod report;
pub mod rumble;
pub mod ufu;
pub mod updater;
