//! A fake Pendragon device driving the companion protocol state machine, used
//! as the device side in tests. Mirrors the QEMU fake Nano's
//! `hid-reply=companion` mode (see `docs/nano.md`): it answers the session,
//! acks and reassembles fragments, and replies to device info and the update
//! subcommands.

use crate::companion::frame::{self, Frame, Reassembler, Sequence};
use crate::companion::protocol;
use std::collections::VecDeque;

/// Fragment payload size the fake device uses for its own replies; matches
/// the 63-byte reports the QEMU fake Nano exposes.
const FRAGMENT_DATA: usize = 57;

/// Session handle the fake device hands out. Must not be the host's own
/// sequence byte (0), which `open_session` always sends.
const HANDLE: u8 = 0x7e;

pub struct FakeNano {
    pub vendor_id: u16,
    pub product_id: u16,
    pub hardware_revision: u16,
    pub firmware_version: [u16; 4],
    pub max_chunk: u16,
    token: [u8; 16],
    slot: u8,
    written: Vec<u8>,
}

impl FakeNano {
    pub fn tala(firmware_version: [u16; 4]) -> Self {
        Self {
            vendor_id: 0x045e,
            product_id: 0x0b4c,
            hardware_revision: 0x0c06,
            firmware_version,
            max_chunk: 0x0800,
            token: *b"fake-nano-token!",
            slot: 0,
            written: Vec::new(),
        }
    }

    pub fn written_data(&self) -> &[u8] {
        &self.written
    }

    fn device_info_response(&self) -> Vec<u8> {
        let mut out = vec![0u8; 0x4e];
        out[0..2].copy_from_slice(&self.vendor_id.to_le_bytes());
        out[2..4].copy_from_slice(&self.product_id.to_le_bytes());
        out[4..6].copy_from_slice(&self.hardware_revision.to_le_bytes());
        out[0x29..0x2d].copy_from_slice(&protocol::CAP_UPDATABLE.to_le_bytes());
        for (i, word) in self.firmware_version.iter().enumerate() {
            out[0x33 + i * 2..0x33 + i * 2 + 2].copy_from_slice(&word.to_le_bytes());
        }
        out
    }

    fn update_response(&mut self, payload: &[u8]) -> Vec<u8> {
        match payload.first().copied() {
            Some(protocol::SUB_STATUS) => {
                let mut out = vec![0u8; 34];
                out[0] = protocol::SUB_STATUS;
                out[2..18].copy_from_slice(&self.token);
                for (i, word) in self.firmware_version.iter().enumerate() {
                    out[18 + i * 2..18 + i * 2 + 2].copy_from_slice(&word.to_le_bytes());
                }
                out[32..34].copy_from_slice(&self.max_chunk.to_le_bytes());
                out
            }
            Some(protocol::SUB_START) if payload.len() >= 27 => {
                self.written.clear();
                vec![protocol::SUB_START, 0, 0, 0, 0, 0, self.slot]
            }
            Some(protocol::SUB_WRITE) if payload.len() >= 5 => {
                let offset = u32::from_le_bytes([payload[2], payload[3], payload[4], 0]) as usize;
                let data = &payload[5..];
                let end = offset + data.len();
                if self.written.len() < end {
                    self.written.resize(end, 0);
                }
                self.written[offset..end].copy_from_slice(data);
                vec![protocol::SUB_WRITE, 0]
            }
            Some(protocol::SUB_FINISH) => {
                // The new version travelled in the start request; a real
                // device would have kept it, but the fake only needs to show
                // the update took effect once finish succeeds.
                vec![protocol::SUB_FINISH, 0]
            }
            _ => vec![payload.first().copied().unwrap_or(0), 13],
        }
    }
}

pub struct CompanionEndpoint {
    pub device: FakeNano,
    reassembler: Reassembler,
    device_seq: Sequence,
    pending: VecDeque<Vec<u8>>,
    /// New firmware version from the last `11` start request, applied when
    /// `13` finishes successfully.
    pending_version: Option<[u16; 4]>,
}

impl CompanionEndpoint {
    pub fn new(device: FakeNano) -> Self {
        Self {
            device,
            reassembler: Reassembler::default(),
            device_seq: Sequence::default(),
            pending: VecDeque::new(),
            pending_version: None,
        }
    }

    /// Feed one incoming HID report payload (after the `0x0c` ID); returns
    /// the report payloads to send back, in order.
    pub fn handle_report(&mut self, bytes: &[u8]) -> Vec<Vec<u8>> {
        if bytes.first() == Some(&0xff) {
            return self.session_accept(bytes);
        }
        let Some(frame) = Frame::parse(bytes) else {
            return Vec::new();
        };
        if frame.kind == frame::TYPE_ACK {
            return self.pending.pop_front().into_iter().collect();
        }
        let mut out = Vec::new();
        if frame.is_fragment() {
            out.push(frame::ack(HANDLE, frame.opcode, frame.seq));
        }
        if let Some(message) = self.reassembler.push(&frame) {
            if frame.opcode == protocol::OP_UPDATE {
                if message.first() == Some(&protocol::SUB_START) && message.len() >= 25 {
                    self.pending_version = Some(std::array::from_fn(|i| {
                        u16::from_le_bytes([message[17 + i * 2], message[17 + i * 2 + 1]])
                    }));
                } else if message.first() == Some(&protocol::SUB_FINISH)
                    && let Some(version) = self.pending_version.take()
                {
                    self.device.firmware_version = version;
                }
            }
            let response = match frame.opcode {
                protocol::OP_DEVICE_INFO => self.device.device_info_response(),
                protocol::OP_UPDATE => self.device.update_response(&message),
                _ => vec![frame.opcode, 13],
            };
            out.extend(self.queue_response(frame.opcode, &response));
        }
        out
    }

    fn session_accept(&mut self, bytes: &[u8]) -> Vec<Vec<u8>> {
        if bytes.len() < 11 {
            return Vec::new();
        }
        let mut reply = vec![0xff, HANDLE];
        reply.extend_from_slice(&bytes[2..6]);
        reply.extend_from_slice(&[2, 0, 22]);
        reply.extend_from_slice(&frame::SESSION_MAX_MESSAGE.to_le_bytes());
        vec![reply]
    }

    /// Queue `payload` as one or more outgoing frames, returning the first
    /// and keeping the rest for release as the host acks them.
    fn queue_response(&mut self, opcode: u8, payload: &[u8]) -> Vec<Vec<u8>> {
        let mut frames =
            frame::fragments(HANDLE, opcode, &mut self.device_seq, payload, FRAGMENT_DATA);
        let first = frames.remove(0);
        self.pending = frames.into();
        vec![first]
    }
}
