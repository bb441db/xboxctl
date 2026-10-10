//! UFU requests and responses. Each one is the payload of a GIP 0x0c message
//! with the system flag; the first byte is the opcode.

use crate::rdhm::Version;

pub const GET_INFO: u8 = 0x00;
pub const GET_VERSION: u8 = 0x01;
pub const BEGIN_UPDATE: u8 = 0x02;
pub const GET_BLOCK_HASH: u8 = 0x03;
pub const WRITE_BLOCK: u8 = 0x04;
pub const COMPLETE_UPDATE: u8 = 0x05;

/// Largest WriteBlock data the host sends.
pub const MAX_WRITE: u32 = 0x1000;
/// Largest UFU request (WriteBlock header + data).
pub const MAX_REQUEST: usize = 8 + MAX_WRITE as usize;

/// Minimum response length per opcode; the Windows driver drops shorter ones.
pub const MIN_RESPONSE: [usize; 6] = [60, 14, 2, 22, 2, 2];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Status {
    Ok = 0,
    InvalidArgument = 1,
    UnknownComponent = 2,
    MustReset = 3,
    ComponentInUse = 4,
    InvalidRange = 5,
    WriteFailure = 6,
    WriteProtected = 7,
    UpdateInProgress = 8,
    UpdateNotInProgress = 9,
    InvalidEepromCsv = 10,
    EepromReadError = 11,
    EepromWriteError = 12,
    ProtocolError = 13,
    IncompatibleVersion = 14,
}

impl Status {
    pub fn from_u8(value: u8) -> Option<Self> {
        use Status::*;
        Some(match value {
            0 => Ok,
            1 => InvalidArgument,
            2 => UnknownComponent,
            3 => MustReset,
            4 => ComponentInUse,
            5 => InvalidRange,
            6 => WriteFailure,
            7 => WriteProtected,
            8 => UpdateInProgress,
            9 => UpdateNotInProgress,
            10 => InvalidEepromCsv,
            11 => EepromReadError,
            12 => EepromWriteError,
            13 => ProtocolError,
            14 => IncompatibleVersion,
            _ => return None,
        })
    }
}

pub fn op_name(op: u8) -> &'static str {
    match op {
        GET_INFO => "GetInfo",
        GET_VERSION => "GetVersion",
        BEGIN_UPDATE => "BeginUpdate",
        GET_BLOCK_HASH => "GetBlockHash",
        WRITE_BLOCK => "WriteBlock",
        COMPLETE_UPDATE => "CompleteUpdate",
        _ => "Unknown",
    }
}

pub fn get_info() -> Vec<u8> {
    vec![GET_INFO]
}

pub fn get_version(component: u32) -> Vec<u8> {
    let mut out = vec![GET_VERSION];
    out.extend_from_slice(&component.to_le_bytes());
    out
}

pub fn begin_update(component: u32, address: u32, size: u32, version: Version) -> Vec<u8> {
    let mut out = vec![BEGIN_UPDATE];
    out.extend_from_slice(&component.to_le_bytes());
    out.extend_from_slice(&address.to_le_bytes());
    out.extend_from_slice(&size.to_le_bytes());
    out.extend_from_slice(&version.to_le());
    out
}

pub fn get_block_hash(address: u32, length: u32) -> Vec<u8> {
    let mut out = vec![GET_BLOCK_HASH];
    out.extend_from_slice(&address.to_le_bytes());
    out.extend_from_slice(&length.to_le_bytes());
    out
}

/// WriteBlock: opcode in a u32 (`04 00 00 00`), address, data.
pub fn write_block(address: u32, data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + data.len());
    out.extend_from_slice(&u32::from(WRITE_BLOCK).to_le_bytes());
    out.extend_from_slice(&address.to_le_bytes());
    out.extend_from_slice(data);
    out
}

pub fn complete_update() -> Vec<u8> {
    vec![COMPLETE_UPDATE]
}

/// GetInfo response fields used by the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Info {
    /// 1 is the legacy original-controller protocol, which is not supported.
    pub protocol: u8,
    pub flags: u8,
    pub vendor_id: u16,
    pub product_id: u16,
    pub max_write: u32,
}

impl Info {
    pub fn parse(payload: &[u8]) -> Option<Self> {
        if payload.len() < MIN_RESPONSE[GET_INFO as usize] {
            return None;
        }
        Some(Self {
            protocol: payload[2],
            flags: payload[3],
            vendor_id: u16::from_le_bytes([payload[4], payload[5]]),
            product_id: u16::from_le_bytes([payload[6], payload[7]]),
            max_write: u32::from_le_bytes(payload[8..12].try_into().unwrap()),
        })
    }

    /// Write size the host uses: capped, rounded down to 4 bytes.
    pub fn block_size(&self) -> u32 {
        self.max_write.min(MAX_WRITE) & !3
    }

    pub fn to_response(self) -> Vec<u8> {
        let mut out = vec![0u8; MIN_RESPONSE[GET_INFO as usize]];
        out[0] = GET_INFO;
        out[2] = self.protocol;
        out[3] = self.flags;
        out[4..6].copy_from_slice(&self.vendor_id.to_le_bytes());
        out[6..8].copy_from_slice(&self.product_id.to_le_bytes());
        out[8..12].copy_from_slice(&self.max_write.to_le_bytes());
        out
    }
}

/// Magic that marks the optional partition size in a GetVersion response.
pub const PARTITION_MAGIC: u32 = 0x1a25_3a45;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ComponentVersion {
    pub component: u32,
    pub version: Version,
    pub partition_size: Option<u32>,
}

impl ComponentVersion {
    pub fn parse(payload: &[u8]) -> Option<Self> {
        if payload.len() < MIN_RESPONSE[GET_VERSION as usize] {
            return None;
        }
        let partition_size = (payload.len() >= 26
            && u32::from_le_bytes(payload[14..18].try_into().unwrap()) == PARTITION_MAGIC)
            .then(|| u32::from_le_bytes(payload[22..26].try_into().unwrap()));
        Some(Self {
            component: u32::from_le_bytes(payload[2..6].try_into().unwrap()),
            version: Version::from_le(&payload[6..14]),
            partition_size,
        })
    }
}
