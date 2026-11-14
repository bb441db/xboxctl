//! Companion Protocol opcodes used for firmware update: device information
//! (`0x2e`) and the update sequence (`0x03`, with subcommands `14`/`11`/`12`/`13`).
//!
//! Field layouts come from the Xbox Accessories SDK and `docs/nano.md`;
//! unexercised bytes (reserved fields, the exact meaning of some status and
//! flag bytes) are left as documented there.

pub const OP_DEVICE_INFO: u8 = 0x2e;
pub const OP_DEVICE_INFO_LEGACY: u8 = 0x23;
/// Older-info fallback opcodes. On Companion versions before 2.0.22, use
/// `0x23`; its reply includes capabilities on retail Nano hardware. `0x1c`
/// is retained as a one-shot fallback for devices that do not answer `0x23`.
pub const OP_DEVICE_INFO_OLD_1: u8 = 0x1c;

pub fn device_info_opcode(version: [u8; 3]) -> u8 {
    if version < [2, 0, 22] {
        OP_DEVICE_INFO_LEGACY
    } else {
        OP_DEVICE_INFO
    }
}
pub const OP_UPDATE: u8 = 0x03;

pub const SUB_STATUS: u8 = 0x14;
pub const SUB_START: u8 = 0x11;
pub const SUB_WRITE: u8 = 0x12;
pub const SUB_FINISH: u8 = 0x13;

/// Minimum reply length the device information request validates.
const DEVICE_INFO_MIN: usize = 0x3b;
/// Minimum reply length for the update status subcommand.
const STATUS_MIN: usize = 34;

/// `2e` request: a timestamp (unused by us) and a fixed trailing word.
pub fn device_info_request() -> Vec<u8> {
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let mut out = timestamp.to_le_bytes().to_vec();
    out.extend_from_slice(&6u32.to_le_bytes());
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceInfo {
    pub vendor_id: u16,
    pub product_id: u16,
    pub hardware_revision: u16,
    pub capabilities: u32,
    pub firmware_version: [u16; 4],
}

/// Capability bit: the device is updatable and shows its update state.
pub const CAP_UPDATABLE: u32 = 1 << 2;

impl DeviceInfo {
    pub fn parse(payload: &[u8]) -> Option<Self> {
        if payload.len() < DEVICE_INFO_MIN {
            return None;
        }
        Some(Self {
            vendor_id: u16::from_le_bytes([payload[0], payload[1]]),
            product_id: u16::from_le_bytes([payload[2], payload[3]]),
            hardware_revision: u16::from_le_bytes([payload[4], payload[5]]),
            capabilities: u32::from_le_bytes(payload[0x29..0x2d].try_into().unwrap()),
            firmware_version: std::array::from_fn(|i| {
                u16::from_le_bytes([payload[0x33 + i * 2], payload[0x33 + i * 2 + 1]])
            }),
        })
    }

    pub fn is_updatable(&self) -> bool {
        self.capabilities & CAP_UPDATABLE != 0
    }
}

/// Reply to the older device-info opcodes (`0x1c`/`0x23`): a different
/// layout than `0x2e`'s, with an ASCII serial number `0x2e` does not carry
/// at all. Decoded 2026-11-14 from a real Nano negotiating protocol 2.0.13
/// (see `research/nano/README.md`).
///
/// `0x1c` and `0x23` are **not** the same reply despite sharing this prefix:
/// a real device's `0x1c` reply is exactly 41 bytes (no capabilities at
/// all), while its `0x23` reply is 51 bytes, with a capabilities `u32` at
/// the same offset (`0x29`) `0x2e`'s reply uses. Still no firmware version
/// either way (unlike `0x2e`) — that must come from the update-status
/// opcode (`03`/`14`) instead. The unlabeled 16 bytes at `0x06`..`0x16` are
/// unidentified either way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegacyDeviceInfo {
    pub vendor_id: u16,
    pub product_id: u16,
    pub hardware_revision: u16,
    /// 14 ASCII bytes, matched against the device's USB `iSerial` string on
    /// real hardware.
    pub serial: [u8; 14],
    /// Present only in the longer (`0x23`-shaped) reply; `None` for the
    /// shorter 41-byte `0x1c` one.
    pub capabilities: Option<u32>,
}

const LEGACY_SERIAL_OFFSET: usize = 0x16;
const LEGACY_SERIAL_LEN: usize = 14;
const LEGACY_DEVICE_INFO_MIN: usize = LEGACY_SERIAL_OFFSET + LEGACY_SERIAL_LEN;
/// Offset of the `u32` capabilities field in the longer reply, same offset
/// `0x2e`'s reply uses.
const LEGACY_CAPABILITIES_OFFSET: usize = 0x29;

impl LegacyDeviceInfo {
    pub fn parse(payload: &[u8]) -> Option<Self> {
        if payload.len() < LEGACY_DEVICE_INFO_MIN {
            return None;
        }
        let capabilities = payload
            .get(LEGACY_CAPABILITIES_OFFSET..LEGACY_CAPABILITIES_OFFSET + 4)
            .map(|bytes| u32::from_le_bytes(bytes.try_into().unwrap()));
        Some(Self {
            vendor_id: u16::from_le_bytes([payload[0], payload[1]]),
            product_id: u16::from_le_bytes([payload[2], payload[3]]),
            hardware_revision: u16::from_le_bytes([payload[4], payload[5]]),
            serial: payload[LEGACY_SERIAL_OFFSET..LEGACY_SERIAL_OFFSET + LEGACY_SERIAL_LEN]
                .try_into()
                .unwrap(),
            capabilities,
        })
    }

    /// `None` when this reply didn't carry capabilities at all (the short
    /// `0x1c` shape).
    pub fn is_updatable(&self) -> Option<bool> {
        self.capabilities.map(|bits| bits & CAP_UPDATABLE != 0)
    }

    /// The serial as text, lossily: it has always been plain ASCII so far.
    pub fn serial_string(&self) -> String {
        String::from_utf8_lossy(&self.serial).into_owned()
    }
}

/// Status byte every update reply starts its second byte with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Ok,
    NotUpdating,
    DeviceFailed,
    OffsetOutOfRange,
    FlashWriteError,
    ImageUnusable,
    Other(u8),
}

impl Status {
    pub fn from_u8(value: u8) -> Self {
        match value {
            0 => Status::Ok,
            5 => Status::NotUpdating,
            6 => Status::DeviceFailed,
            7 => Status::OffsetOutOfRange,
            8 => Status::FlashWriteError,
            9 => Status::ImageUnusable,
            other => Status::Other(other),
        }
    }
}

/// `14`: ask for the update token, mode and chunk size. Request is the bare
/// subcommand byte.
pub fn status_request() -> Vec<u8> {
    vec![SUB_STATUS]
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UpdateStatus {
    pub status: Status,
    pub token: [u8; 16],
    pub running_version: [u16; 4],
    /// Bit 0: an update is already in progress.
    pub flags: u8,
    /// Echoed back in the `11` start request; `5` means mode 5 (opcode `15`
    /// instead of `11`), not implemented here.
    pub mode: u8,
    pub max_chunk: u16,
    /// Number of firmware bytes the device has staged (present in full replies).
    pub bytes_written: Option<u32>,
}

impl UpdateStatus {
    pub fn parse(payload: &[u8]) -> Option<Self> {
        if payload.len() < STATUS_MIN || payload[0] != SUB_STATUS {
            return None;
        }
        Some(Self {
            status: Status::from_u8(payload[1]),
            token: payload[2..18].try_into().unwrap(),
            running_version: std::array::from_fn(|i| {
                u16::from_le_bytes([payload[18 + i * 2], payload[18 + i * 2 + 1]])
            }),
            flags: payload[26],
            mode: payload[27],
            max_chunk: u16::from_le_bytes([payload[32], payload[33]]),
            bytes_written: payload
                .get(60..64)
                .map(|bytes| u32::from_le_bytes(bytes.try_into().unwrap())),
        })
    }
}

/// `11`: start the update. `foreground` sets the flags nibble the SDK uses
/// for a foreground (as opposed to background-service-driven) update.
pub fn start_request(
    token: [u8; 16],
    new_version: [u16; 4],
    foreground: bool,
    mode: u8,
) -> Vec<u8> {
    let mut out = vec![SUB_START];
    out.extend_from_slice(&token);
    for word in new_version {
        out.extend_from_slice(&word.to_le_bytes());
    }
    out.push(if foreground { 0x10 } else { 0x00 });
    out.push(mode);
    out.push(0);
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StartReply {
    pub status: Status,
    pub resume_offset: u32,
    pub slot: u8,
}

impl StartReply {
    pub fn parse(payload: &[u8]) -> Option<Self> {
        if payload.len() < 7 || payload[0] != SUB_START {
            return None;
        }
        Some(Self {
            status: Status::from_u8(payload[1]),
            resume_offset: u32::from_le_bytes(payload[2..6].try_into().unwrap()),
            slot: payload[6],
        })
    }
}

/// `12`: write one chunk of the image at `offset` (a 24-bit value; the host
/// never writes past 0xff_ffff with Nano-sized images).
pub fn write_request(slot: u8, offset: u32, data: &[u8]) -> Vec<u8> {
    assert!(offset < 1 << 24, "offset does not fit 24 bits");
    let mut out = vec![SUB_WRITE, slot];
    out.extend_from_slice(&offset.to_le_bytes()[..3]);
    out.extend_from_slice(data);
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WriteReply {
    pub status: Status,
    /// Bit 0 is set on successful write replies from a retail Nano; the SDK
    /// records it and continues writing.
    pub flags: u8,
}

impl WriteReply {
    pub fn parse(payload: &[u8]) -> Option<Self> {
        if payload.len() < 2 || payload[0] != SUB_WRITE {
            return None;
        }
        Some(Self {
            status: Status::from_u8(payload[1]),
            flags: payload.get(2).copied().unwrap_or(0),
        })
    }

    pub fn parse_for_version(payload: &[u8], version: [u8; 3]) -> Option<Self> {
        if version >= [2, 0, 8] && payload.len() < 3 {
            return None;
        }
        Self::parse(payload)
    }
}

/// `13`: finish. `foreground` is logged by the SDK as "yes"/"no"; consent
/// (`0x04`) is not implemented here since a foreground update does not wait
/// for it.
pub fn finish_request_with_flags(slot: u8, flags: u8) -> Vec<u8> {
    vec![SUB_FINISH, slot, flags]
}

pub fn finish_status(payload: &[u8]) -> Option<Status> {
    if payload.len() < 2 || payload[0] != SUB_FINISH {
        return None;
    }
    Some(Status::from_u8(payload[1]))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real `0x1c` reply, captured 2026-11-14 against a retail Nano
    /// (`045e:0b4e`, serial `0D336JY2626BBP`) negotiating protocol 2.0.13;
    /// see `research/nano/README.md`. Reported product ID is `0x0b4c`
    /// (TALA's dev-board PID), not the USB descriptor's `0x0b4e`. Shorter
    /// than `0x23`'s reply below despite sharing the same prefix: no
    /// capabilities field at all.
    const REAL_LEGACY_DEVICE_INFO: [u8; 41] = [
        0x5e, 0x04, 0x4c, 0x0b, 0x06, 0x0c, 0x9c, 0x12, 0xe9, 0x14, 0x81, 0x03, 0x3d, 0xb9, 0xc2,
        0x8e, 0xad, 0x90, 0xda, 0x88, 0xf7, 0x84, 0x30, 0x44, 0x33, 0x33, 0x36, 0x4a, 0x59, 0x32,
        0x36, 0x32, 0x36, 0x42, 0x42, 0x50, 0x02, 0x00, 0x00, 0x00, 0x00,
    ];

    /// The same device's real `0x23` reply, captured 2026-11-14 right after
    /// the 2e-opcode/1c-opcode reordering experiment in
    /// `DEVICE_INFO_OPCODES`: identical 41-byte prefix, plus a 10-byte
    /// suffix with a `u32` capabilities field at `0x29` (same offset `0x2e`
    /// uses), value `0x3ff` here.
    const REAL_LEGACY_DEVICE_INFO_23: [u8; 51] = [
        0x5e, 0x04, 0x4c, 0x0b, 0x06, 0x0c, 0x9c, 0x12, 0xe9, 0x14, 0x81, 0x03, 0x3d, 0xb9, 0xc2,
        0x8e, 0xad, 0x90, 0xda, 0x88, 0xf7, 0x84, 0x30, 0x44, 0x33, 0x33, 0x36, 0x4a, 0x59, 0x32,
        0x36, 0x32, 0x36, 0x42, 0x42, 0x50, 0x02, 0x00, 0x00, 0x00, 0x00, 0xff, 0x03, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    ];

    #[test]
    fn parses_real_legacy_device_info() {
        let info = LegacyDeviceInfo::parse(&REAL_LEGACY_DEVICE_INFO).unwrap();
        assert_eq!(info.vendor_id, 0x045e);
        assert_eq!(info.product_id, 0x0b4c);
        assert_eq!(info.hardware_revision, 0x0c06);
        assert_eq!(info.serial_string(), "0D336JY2626BBP");
        assert_eq!(info.capabilities, None);
        assert_eq!(info.is_updatable(), None);
    }

    #[test]
    fn parses_real_legacy_device_info_0x23_with_capabilities() {
        let info = LegacyDeviceInfo::parse(&REAL_LEGACY_DEVICE_INFO_23).unwrap();
        assert_eq!(info.serial_string(), "0D336JY2626BBP");
        assert_eq!(info.capabilities, Some(0x3ff));
        assert_eq!(info.is_updatable(), Some(true));
    }

    #[test]
    fn parses_device_info() {
        let mut payload = vec![0u8; 0x4e];
        payload[0..2].copy_from_slice(&0x0b4cu16.to_le_bytes());
        payload[2..4].copy_from_slice(&0x045eu16.to_le_bytes());
        payload[4..6].copy_from_slice(&0x0c06u16.to_le_bytes());
        payload[0x29..0x2d].copy_from_slice(&CAP_UPDATABLE.to_le_bytes());
        payload[0x33..0x3b].copy_from_slice(&[0x06, 0, 0x13, 0, 0x32, 0, 0, 0]);
        let info = DeviceInfo::parse(&payload).unwrap();
        assert_eq!(info.hardware_revision, 0x0c06);
        assert!(info.is_updatable());
        assert_eq!(info.firmware_version, [6, 19, 50, 0]);
    }

    #[test]
    fn parses_update_status() {
        let mut payload = vec![0u8; STATUS_MIN];
        payload[0] = SUB_STATUS;
        payload[2..18].copy_from_slice(&[0xaa; 16]);
        payload[18..26].copy_from_slice(&[6, 0, 19, 0, 49, 0, 0, 0]);
        payload[32..34].copy_from_slice(&0x0800u16.to_le_bytes());
        let status = UpdateStatus::parse(&payload).unwrap();
        assert_eq!(status.status, Status::Ok);
        assert_eq!(status.token, [0xaa; 16]);
        assert_eq!(status.running_version, [6, 19, 49, 0]);
        assert_eq!(status.max_chunk, 0x0800);
    }

    #[test]
    fn round_trips_start_and_write() {
        let request = start_request([1; 16], [6, 19, 50, 0], true, 0);
        assert_eq!(request[0], SUB_START);
        assert_eq!(request[25], 0x10); // foreground flag nibble
        let write = write_request(3, 0x12_3456, &[0xde, 0xad]);
        assert_eq!(&write[1..5], &[3, 0x56, 0x34, 0x12]);
    }
}
