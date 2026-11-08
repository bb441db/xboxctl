//! Companion Protocol opcodes used for firmware update: device information
//! (`0x2e`) and the update sequence (`0x03`, with subcommands `14`/`11`/`12`/`13`).
//!
//! Field layouts come from the Xbox Accessories SDK and `docs/nano.md`;
//! unexercised bytes (reserved fields, the exact meaning of some status and
//! flag bytes) are left as documented there.

pub const OP_DEVICE_INFO: u8 = 0x2e;
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
    let mut out = vec![0u8; 12];
    out.extend_from_slice(&0x3bu32.to_le_bytes());
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
    /// Bit 0: the device wants a restart before continuing. Not yet seen in
    /// testing; if it occurs, the caller should treat it as a hard error
    /// rather than silently pressing on.
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
}

/// `13`: finish. `foreground` is logged by the SDK as "yes"/"no"; consent
/// (`0x04`) is not implemented here since a foreground update does not wait
/// for it.
pub fn finish_request(slot: u8, foreground: bool) -> Vec<u8> {
    vec![SUB_FINISH, slot, if foreground { 1 } else { 0 }]
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
