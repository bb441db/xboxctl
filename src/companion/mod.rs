//! Microsoft's Companion Protocol, used over HID by the Xbox Nano Controller
//! and the rest of the Pendragon accessory family instead of GIP.
//!
//! Reverse engineered from the Xbox Accessories app's SDK and checked against
//! a QEMU-emulated Nano (see `docs/nano.md`); not yet checked against
//! physical hardware.

/// Fake device used as the device side in tests; the emulator for manual
/// testing is the QEMU `usb-xbox-gip` device in `hid-reply=companion` mode.
#[cfg(test)]
pub mod device;
pub mod firmware;
pub mod frame;
pub mod protocol;
pub mod updater;
