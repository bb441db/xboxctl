//! Device side of UFU: a controller with simulated flash. Ported from the QEMU
//! `usb-xbox-gip` device, which the Windows updater accepted end to end.

use crate::frame::{self, Packet, Reassembler, Sequence};
use crate::rdhm::Version;
use crate::ufu::{self, Info, Status};
use sha1::{Digest, Sha1};
use std::collections::BTreeMap;

/// MERL (045e:0b12) flash: every component target address fits in here.
pub const MERL_FLASH_BASE: u32 = 0x1000_0000;
pub const MERL_FLASH_SIZE: u32 = 0xa6000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Completed {
    pub component: u32,
    pub address: u32,
    pub size: u32,
    pub version: Version,
}

#[derive(Debug, Clone, Copy)]
struct Active {
    component: u32,
    address: u32,
    size: u32,
    version: Version,
}

/// UFU request handler with simulated flash.
#[derive(Debug, Clone)]
pub struct UfuDevice {
    info: Info,
    flash_base: u32,
    flash: Vec<u8>,
    versions: BTreeMap<u32, Version>,
    active: Option<Active>,
    completed: Vec<Completed>,
}

impl UfuDevice {
    /// A MERL controller whose components 1-4 all report `version`.
    pub fn merlin(version: Version) -> Self {
        Self {
            info: Info {
                // Anything but 1 (legacy); accepted by Windows.
                protocol: 2,
                flags: 0,
                vendor_id: 0x045e,
                product_id: 0x0b12,
                max_write: ufu::MAX_WRITE,
            },
            flash_base: MERL_FLASH_BASE,
            flash: vec![0xff; MERL_FLASH_SIZE as usize],
            versions: (1..=4).map(|id| (id, version)).collect(),
            active: None,
            completed: Vec::new(),
        }
    }

    pub fn version(&self, component: u32) -> Option<Version> {
        self.versions.get(&component).copied()
    }

    /// Flash contents at `address`, if the range is inside flash.
    pub fn flash(&self, address: u32, size: u32) -> Option<&[u8]> {
        let start = address.checked_sub(self.flash_base)? as usize;
        self.flash.get(start..start.checked_add(size as usize)?)
    }

    /// Components finished since the last call.
    pub fn take_completed(&mut self) -> Vec<Completed> {
        std::mem::take(&mut self.completed)
    }

    fn flash_mut(&mut self, address: u32, size: u32) -> Option<&mut [u8]> {
        let start = address.checked_sub(self.flash_base)? as usize;
        self.flash.get_mut(start..start.checked_add(size as usize)?)
    }

    /// Handle one UFU request and return the response payload. Error responses
    /// are padded to the host's minimum length so it sees the status.
    pub fn handle(&mut self, request: &[u8]) -> Option<Vec<u8>> {
        let &op = request.first()?;
        let u32_at = |offset: usize| {
            request
                .get(offset..offset + 4)
                .map(|bytes| u32::from_le_bytes(bytes.try_into().unwrap()))
        };
        let mut response = match op {
            ufu::GET_INFO => self.info.to_response(),
            _ => vec![op, Status::Ok as u8],
        };
        match op {
            ufu::GET_INFO => {}
            ufu::GET_VERSION => {
                response.resize(ufu::MIN_RESPONSE[op as usize], 0);
                match (request.len(), u32_at(1)) {
                    (5, Some(id)) => {
                        response[2..6].copy_from_slice(&id.to_le_bytes());
                        match self.versions.get(&id) {
                            Some(version) => response[6..14].copy_from_slice(&version.to_le()),
                            None => response[1] = Status::UnknownComponent as u8,
                        }
                    }
                    _ => response[1] = Status::InvalidArgument as u8,
                }
            }
            ufu::BEGIN_UPDATE => {
                if request.len() != 21 {
                    response[1] = Status::InvalidArgument as u8;
                } else {
                    let (id, address, size) = (u32_at(1)?, u32_at(5)?, u32_at(9)?);
                    let version = Version::from_le(&request[13..21]);
                    if !self.versions.contains_key(&id) {
                        response[1] = Status::UnknownComponent as u8;
                    } else if let Some(region) = self.flash_mut(address, size) {
                        // Erase like real flash so that every block is rewritten.
                        region.fill(0xff);
                        self.active = Some(Active { component: id, address, size, version });
                    } else {
                        response[1] = Status::InvalidRange as u8;
                    }
                }
            }
            ufu::GET_BLOCK_HASH => {
                response.resize(ufu::MIN_RESPONSE[op as usize], 0);
                match (request.len(), u32_at(1), u32_at(5)) {
                    (9, Some(address), Some(size)) => match self.flash(address, size) {
                        Some(data) => response[2..22].copy_from_slice(&Sha1::digest(data)),
                        None => response[1] = Status::InvalidRange as u8,
                    },
                    _ => response[1] = Status::InvalidArgument as u8,
                }
            }
            ufu::WRITE_BLOCK => match (self.active, u32_at(4)) {
                (_, None) => response[1] = Status::InvalidArgument as u8,
                (None, Some(_)) => response[1] = Status::UpdateNotInProgress as u8,
                (Some(active), Some(address)) => {
                    let data = &request[8..];
                    let end = u64::from(address) + data.len() as u64;
                    if address < active.address
                        || end > u64::from(active.address) + u64::from(active.size)
                    {
                        response[1] = Status::InvalidRange as u8;
                    } else if let Some(region) = self.flash_mut(address, data.len() as u32) {
                        region.copy_from_slice(data);
                    } else {
                        response[1] = Status::InvalidRange as u8;
                    }
                }
            },
            ufu::COMPLETE_UPDATE => match self.active.take() {
                None => response[1] = Status::UpdateNotInProgress as u8,
                Some(active) => {
                    self.versions.insert(active.component, active.version);
                    self.completed.push(Completed {
                        component: active.component,
                        address: active.address,
                        size: active.size,
                        version: active.version,
                    });
                }
            },
            _ => response[1] = Status::ProtocolError as u8,
        }
        Some(response)
    }
}

/// GIP 0x0c endpoint: ACKs, fragment reassembly and responses around a
/// [`UfuDevice`].
#[derive(Debug)]
pub struct UfuEndpoint {
    pub device: UfuDevice,
    reassembler: Reassembler,
    sequence: Sequence,
}

impl UfuEndpoint {
    pub fn new(device: UfuDevice) -> Self {
        Self { device, reassembler: Reassembler::default(), sequence: Sequence::default() }
    }

    /// Handle a host GIP 0x0c packet; returns device packets to send.
    pub fn handle_packet(&mut self, packet: &Packet) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        let request = if packet.is_fragment() {
            self.reassembler.push(packet, ufu::MAX_REQUEST)
        } else {
            Some(packet.payload.clone())
        };
        if packet.wants_ack() {
            let (received, remaining) = if packet.is_fragment() {
                self.reassembler.progress()
            } else {
                (packet.payload.len() as u32, 0)
            };
            out.push(frame::ack(packet, received, remaining));
        }
        if let Some(response) = request.and_then(|request| self.device.handle(&request)) {
            out.push(frame::packet(
                frame::CMD_FIRMWARE,
                frame::FLAG_SYSTEM,
                self.sequence.take(),
                &response,
            ));
        }
        out
    }
}
