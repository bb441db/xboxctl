//! Xbox Gaming Input Protocol (GIP) framing and the UFU firmware update
//! protocol, shared by `xboxctl` (host side) and `xbox-virtual-device`
//! (device side).
//!
//! The UFU protocol is documented in `docs/firmware-update-host.md`. It was
//! reverse engineered from Windows and verified by updating the QEMU-emulated
//! controller with the Xbox Accessories app.

pub mod device;
pub mod frame;
pub mod rdhm;
pub mod ufu;
pub mod updater;
