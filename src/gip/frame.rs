//! GIP packet headers, fragmentation and Protocol Control ACKs (MS-GIPUSB).

/// Largest GIP packet on the 64-byte interrupt endpoints.
pub const PACKET_MAX: usize = 64;

pub const FLAG_ACME: u8 = 0x10;
pub const FLAG_SYSTEM: u8 = 0x20;
pub const FLAG_INIT_FRAG: u8 = 0x40;
pub const FLAG_FRAGMENT: u8 = 0x80;

pub const CMD_ACK: u8 = 0x01;
pub const CMD_HELLO: u8 = 0x02;
pub const CMD_STATUS: u8 = 0x03;
pub const CMD_SET_DEVICE_STATE: u8 = 0x05;
pub const CMD_RUMBLE: u8 = 0x09;
pub const CMD_FIRMWARE: u8 = 0x0c;

pub const STATE_START: u8 = 0x00;
pub const STATE_RESET: u8 = 0x07;

/// Fragment headers are always 6 bytes; MS-GIPUSB requires host-to-device
/// headers to have an even length.
const FRAGMENT_HEADER: usize = 6;
/// Data bytes per fragment.
pub const FRAGMENT_DATA: usize = PACKET_MAX - FRAGMENT_HEADER;

/// One received GIP packet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Packet {
    pub command: u8,
    pub flags: u8,
    pub sequence: u8,
    /// For fragments: total length (first fragment) or offset (later ones).
    pub total_or_offset: Option<u32>,
    pub payload: Vec<u8>,
}

impl Packet {
    pub fn parse(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < 4 {
            return None;
        }
        let mut pos = 3;
        let length = read_varint(bytes, &mut pos)? as usize;
        let total_or_offset = if bytes[1] & FLAG_FRAGMENT != 0 {
            Some(read_varint(bytes, &mut pos)?)
        } else {
            None
        };
        let payload = bytes.get(pos..pos.checked_add(length)?)?.to_vec();
        Some(Self {
            command: bytes[0],
            flags: bytes[1],
            sequence: bytes[2],
            total_or_offset,
            payload,
        })
    }

    pub fn wants_ack(&self) -> bool {
        self.flags & FLAG_ACME != 0 && self.command != CMD_ACK
    }

    pub fn is_fragment(&self) -> bool {
        self.flags & FLAG_FRAGMENT != 0
    }

    #[cfg(test)]
    pub fn is_first_fragment(&self) -> bool {
        self.is_fragment() && self.flags & FLAG_INIT_FRAG != 0
    }
}

/// Little-endian base-128 varint, at most four bytes.
pub fn read_varint(bytes: &[u8], pos: &mut usize) -> Option<u32> {
    let mut value = 0u32;
    for shift in [0, 7, 14, 21] {
        let byte = *bytes.get(*pos)?;
        *pos += 1;
        value |= u32::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some(value);
        }
    }
    None
}

/// Two-byte varint used to pad fragment headers to an even length.
fn padded_varint(value: u32) -> [u8; 2] {
    debug_assert!(value < 1 << 14);
    [(value as u8 & 0x7f) | 0x80, (value >> 7) as u8]
}

/// Build an unfragmented packet. `payload` must fit one packet.
pub fn packet(command: u8, flags: u8, sequence: u8, payload: &[u8]) -> Vec<u8> {
    assert!(payload.len() <= PACKET_MAX - 4, "payload needs fragmenting");
    let mut out = vec![command, flags, sequence, payload.len() as u8];
    out.extend_from_slice(payload);
    out
}

/// Build one fragment of a large message. `offset` is the fragment's position
/// in the full message; the first fragment carries `total` instead.
pub fn fragment(
    command: u8,
    flags: u8,
    sequence: u8,
    total: u32,
    offset: u32,
    data: &[u8],
    ack: bool,
) -> Vec<u8> {
    assert!(data.len() <= FRAGMENT_DATA);
    let mut fragment_flags = flags | FLAG_FRAGMENT;
    let tlo = if offset == 0 && !data.is_empty() {
        fragment_flags |= FLAG_INIT_FRAG;
        total
    } else {
        offset
    };
    if ack {
        fragment_flags |= FLAG_ACME;
    }
    let mut out = vec![command, fragment_flags, sequence, data.len() as u8];
    out.extend_from_slice(&padded_varint(tlo));
    out.extend_from_slice(data);
    out
}

/// The zero-length fragment that ends a fragmented message, as sent by the
/// physical controller after its metadata fragments.
pub fn end_of_fragments(command: u8, flags: u8, sequence: u8, total: u32) -> Vec<u8> {
    fragment(
        command,
        flags & !FLAG_ACME,
        sequence,
        total,
        total,
        &[],
        false,
    )
}

/// Protocol Control ACK for a received packet.
///
/// Payload: reserved, acknowledged command, options, bytes received (u16),
/// two padding bytes, bytes remaining (u16). This matches the physical
/// controller's ACKs and the Windows host's.
pub fn ack(acked: &Packet, sequence_received: u32, remaining: u32) -> Vec<u8> {
    let mut payload = [0u8; 9];
    payload[1] = acked.command;
    payload[2] = FLAG_SYSTEM | (acked.flags & 0x0f);
    payload[3..5].copy_from_slice(&(sequence_received as u16).to_le_bytes());
    payload[7..9].copy_from_slice(&(remaining as u16).to_le_bytes());
    packet(CMD_ACK, FLAG_SYSTEM, acked.sequence, &payload)
}

/// Fields of a received ACK.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ack {
    pub command: u8,
    pub received: u32,
    pub remaining: u32,
}

impl Ack {
    pub fn parse(packet: &Packet) -> Option<Self> {
        if packet.command != CMD_ACK || packet.payload.len() < 9 {
            return None;
        }
        let p = &packet.payload;
        Some(Self {
            command: p[1],
            received: u32::from(u16::from_le_bytes([p[3], p[4]])),
            remaining: u32::from(u16::from_le_bytes([p[7], p[8]])),
        })
    }
}

/// GIP Hello (0x02) payload: the device announcing itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hello {
    pub device_id: [u8; 8],
    pub vendor_id: u16,
    pub product_id: u16,
    pub firmware: crate::gip::rdhm::Version,
    /// Last eight bytes; the layout is not known (`08 04 01 00 01 00 01 00`
    /// on the captured controller).
    pub hardware: [u8; 8],
}

impl Hello {
    pub fn parse(packet: &Packet) -> Option<Self> {
        let p = &packet.payload;
        if packet.command != CMD_HELLO || p.len() < 28 {
            return None;
        }
        Some(Self {
            device_id: p[0..8].try_into().unwrap(),
            vendor_id: u16::from_le_bytes([p[8], p[9]]),
            product_id: u16::from_le_bytes([p[10], p[11]]),
            firmware: crate::gip::rdhm::Version::from_le(&p[12..20]),
            hardware: p[20..28].try_into().unwrap(),
        })
    }
}

/// Sequence numbers run 1..=255; zero is skipped.
#[derive(Debug, Clone, Copy)]
pub struct Sequence(u8);

impl Default for Sequence {
    fn default() -> Self {
        Self(1)
    }
}

impl Sequence {
    pub fn take(&mut self) -> u8 {
        let value = self.0;
        self.0 = self.0.wrapping_add(1).max(1);
        value
    }
}

/// Reassembles one fragmented message at a time.
#[cfg(test)]
#[derive(Debug, Default)]
pub struct Reassembler {
    buffer: Vec<u8>,
    total: usize,
    filled: usize,
    active: bool,
}

#[cfg(test)]
impl Reassembler {
    /// Feed a fragment. Returns the full message once every byte arrived.
    pub fn push(&mut self, packet: &Packet, max_total: usize) -> Option<Vec<u8>> {
        let tlo = packet.total_or_offset? as usize;
        let offset = if packet.is_first_fragment() {
            if tlo > max_total {
                self.active = false;
                return None;
            }
            self.buffer = vec![0; tlo];
            self.total = tlo;
            self.filled = 0;
            self.active = true;
            0
        } else {
            tlo
        };
        // Only contiguous data is accepted; later fragments are discarded and
        // must be resent (MS-GIPUSB reliable message rules).
        if !self.active || packet.payload.is_empty() || offset > self.filled {
            return None;
        }
        let end = offset.checked_add(packet.payload.len())?;
        if end > self.total {
            return None;
        }
        self.buffer[offset..end].copy_from_slice(&packet.payload);
        self.filled = self.filled.max(end);
        if self.filled == self.total {
            self.active = false;
            return Some(std::mem::take(&mut self.buffer));
        }
        None
    }

    /// Bytes received so far and bytes outstanding, for ACKs.
    pub fn progress(&self) -> (u32, u32) {
        (self.filled as u32, (self.total - self.filled) as u32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_fragment_matches_windows() {
        // Windows' first WriteBlock fragment: 0c f0 <seq> 3a 88 20 ...
        let data = [0u8; FRAGMENT_DATA];
        let f = fragment(CMD_FIRMWARE, FLAG_SYSTEM, 0x10, 0x1008, 0, &data, true);
        assert_eq!(&f[..6], &[0x0c, 0xf0, 0x10, 0x3a, 0x88, 0x20]);
        assert_eq!(f.len(), PACKET_MAX);
    }

    #[test]
    fn end_marker_matches_controller_metadata() {
        // The controller ended its 0x123-byte metadata with 04 a0 03 00 a3 02.
        assert_eq!(
            end_of_fragments(0x04, FLAG_SYSTEM, 0x03, 0x123),
            [0x04, 0xa0, 0x03, 0x00, 0xa3, 0x02]
        );
    }

    #[test]
    fn parses_captured_hello() {
        let bytes = [
            0x02, 0x20, 0xe4, 0x1c, 0x7e, 0xed, 0x8d, 0xa3, 0x8d, 0xaa, 0x00, 0x00, 0x5e, 0x04,
            0x12, 0x0b, 0x05, 0x00, 0x17, 0x00, 0x06, 0x00, 0x00, 0x00, 0x08, 0x04, 0x01, 0x00,
            0x01, 0x00, 0x01, 0x00,
        ];
        let hello = Hello::parse(&Packet::parse(&bytes).unwrap()).unwrap();
        assert_eq!((hello.vendor_id, hello.product_id), (0x045e, 0x0b12));
        assert_eq!(hello.firmware.to_string(), "5.23.6.0");
        assert_eq!(
            hello.hardware,
            [0x08, 0x04, 0x01, 0x00, 0x01, 0x00, 0x01, 0x00]
        );
    }

    #[test]
    fn parses_captured_ack() {
        let p = Packet::parse(&[
            0x01, 0x20, 0x03, 0x09, 0x00, 0x04, 0x20, 0x3a, 0x00, 0x00, 0x00, 0xe9, 0x00,
        ])
        .unwrap();
        assert_eq!(
            Ack::parse(&p),
            Some(Ack {
                command: 0x04,
                received: 0x3a,
                remaining: 0xe9
            })
        );
    }

    #[test]
    fn reassembles_fragments() {
        let message: Vec<u8> = (0..200u8).collect();
        let mut r = Reassembler::default();
        let mut out = None;
        for (i, chunk) in message.chunks(FRAGMENT_DATA).enumerate() {
            let f = fragment(
                0x0c,
                FLAG_SYSTEM,
                1,
                200,
                (i * FRAGMENT_DATA) as u32,
                chunk,
                false,
            );
            out = r.push(&Packet::parse(&f).unwrap(), 0x2000);
        }
        assert_eq!(out, Some(message));
    }
}
