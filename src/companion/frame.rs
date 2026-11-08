//! Framing for Microsoft's Companion Protocol, used by Pendragon accessories
//! (the Xbox Nano Controller and relatives) instead of GIP. Every packet is a
//! HID report with report ID `0x0c`; `report_len` below is the payload that
//! follows that ID, which is the HID report's byte count minus one.
//!
//! Reverse engineered from the Xbox Accessories app's `xaccessories_sdk_uwp.dll`
//! (see `docs/nano.md`) and checked against a QEMU-emulated Nano, not yet
//! against physical hardware.

/// HID report ID the companion collection speaks on.
pub const REPORT_ID: u8 = 0x0c;

pub const TYPE_MIDDLE: u8 = 0x01;
pub const TYPE_ACK: u8 = 0x02;
pub const TYPE_FIRST: u8 = 0x03;
pub const TYPE_LAST: u8 = 0x04;
pub const TYPE_SINGLE: u8 = 0x05;

/// Control packet type (session open/accept), sent and received unwrapped by
/// the `0x0c` report but distinguished from data frames by this leading byte.
const TYPE_SESSION: u8 = 0xff;

/// Companion Protocol version the host offers; the device must answer with at
/// least major version 2.
const VERSION: [u8; 3] = [2, 0, 22];

/// Maximum size of one reassembled message that the host advertises.
pub const SESSION_MAX_MESSAGE: u16 = 0x1002;

/// Bytes of data-frame header before the payload: type, handle, opcode, seq,
/// length.
const DATA_HEADER: usize = 5;

/// Open a session: `ff <seq=0> <session id:u32> <version> <max:u16>`.
pub fn open_session(session_id: u32) -> Vec<u8> {
    let mut out = vec![TYPE_SESSION, 0];
    out.extend_from_slice(&session_id.to_le_bytes());
    out.extend_from_slice(&VERSION);
    out.extend_from_slice(&SESSION_MAX_MESSAGE.to_le_bytes());
    out
}

/// The device's reply to `open_session`: the same session ID and a sequence
/// byte that differs from the one the host sent (that byte becomes the
/// session handle), with the device's own version and max message size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionAccept {
    pub handle: u8,
    pub session_id: u32,
    pub version_major: u8,
    pub max_message: u16,
}

impl SessionAccept {
    /// `sent_seq` is the sequence byte the host's own open_session used (0);
    /// a reply that echoes it is a duplicate of the host's own packet, not an
    /// accept, and must be ignored.
    pub fn parse(bytes: &[u8], session_id: u32, sent_seq: u8) -> Option<Self> {
        if bytes.len() < 11 || bytes[0] != TYPE_SESSION {
            return None;
        }
        let seq = bytes[1];
        if seq == sent_seq {
            return None;
        }
        let replied_id = u32::from_le_bytes(bytes[2..6].try_into().unwrap());
        if replied_id != session_id {
            return None;
        }
        let version_major = bytes[6];
        if version_major < 2 {
            return None;
        }
        Some(Self {
            handle: seq,
            session_id,
            version_major,
            max_message: u16::from_le_bytes([bytes[9], bytes[10]]),
        })
    }
}

/// One parsed data or ack frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub kind: u8,
    pub handle: u8,
    pub opcode: u8,
    pub seq: u8,
    /// Ack frames carry no payload; `seq` already is the acked sequence + 1.
    pub payload: Vec<u8>,
}

impl Frame {
    pub fn parse(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < 4 {
            return None;
        }
        let kind = bytes[0];
        if kind == TYPE_ACK {
            return Some(Self {
                kind,
                handle: bytes[1],
                opcode: bytes[2],
                seq: bytes[3],
                payload: Vec::new(),
            });
        }
        if bytes.len() < DATA_HEADER {
            return None;
        }
        let length = bytes[4] as usize;
        let payload = bytes.get(DATA_HEADER..DATA_HEADER + length)?;
        Some(Self {
            kind,
            handle: bytes[1],
            opcode: bytes[2],
            seq: bytes[3],
            payload: payload.to_vec(),
        })
    }

    pub fn is_fragment(&self) -> bool {
        matches!(self.kind, TYPE_FIRST | TYPE_MIDDLE | TYPE_LAST)
    }
}

/// Build one data frame. `data` must fit the report (`report_len - DATA_HEADER`).
fn data_frame(kind: u8, handle: u8, opcode: u8, seq: u8, data: &[u8]) -> Vec<u8> {
    let mut out = vec![kind, handle, opcode, seq, data.len() as u8];
    out.extend_from_slice(data);
    out
}

/// `02 <handle> <opcode> <seq + 1>`: acknowledges the frame and asks for the
/// next sequence number.
pub fn ack(handle: u8, opcode: u8, acked_seq: u8) -> Vec<u8> {
    vec![TYPE_ACK, handle, opcode, acked_seq.wrapping_add(1)]
}

/// Split `message` into one or more data frames addressed to `handle` for
/// `opcode`, each at most `max_data` payload bytes. A message that fits in
/// one frame is sent as `TYPE_SINGLE` and needs no per-fragment ack; a longer
/// one is `TYPE_FIRST`, `TYPE_MIDDLE`* and `TYPE_LAST`, each needing one.
pub fn fragments(
    handle: u8,
    opcode: u8,
    seq: &mut Sequence,
    message: &[u8],
    max_data: usize,
) -> Vec<Vec<u8>> {
    assert!(max_data > 0);
    if message.len() <= max_data {
        return vec![data_frame(TYPE_SINGLE, handle, opcode, seq.take(), message)];
    }
    let chunks: Vec<&[u8]> = message.chunks(max_data).collect();
    chunks
        .iter()
        .enumerate()
        .map(|(index, chunk)| {
            let kind = if index == 0 {
                TYPE_FIRST
            } else if index + 1 == chunks.len() {
                TYPE_LAST
            } else {
                TYPE_MIDDLE
            };
            data_frame(kind, handle, opcode, seq.take(), chunk)
        })
        .collect()
}

/// Per-message sequence counter; wraps like the GIP one does. The device
/// expects it to advance by one per frame sent.
#[derive(Debug, Clone, Copy, Default)]
pub struct Sequence(u8);

impl Sequence {
    pub fn take(&mut self) -> u8 {
        let value = self.0;
        self.0 = self.0.wrapping_add(1);
        value
    }
}

/// Reassembles one fragmented message.
#[derive(Debug, Default)]
pub struct Reassembler {
    buffer: Vec<u8>,
    active: bool,
}

impl Reassembler {
    /// Feed one data frame. Returns the full message once the last fragment
    /// (or a `TYPE_SINGLE` frame) arrives.
    pub fn push(&mut self, frame: &Frame) -> Option<Vec<u8>> {
        match frame.kind {
            TYPE_SINGLE => Some(frame.payload.clone()),
            TYPE_FIRST => {
                self.buffer = frame.payload.clone();
                self.active = true;
                None
            }
            TYPE_MIDDLE if self.active => {
                self.buffer.extend_from_slice(&frame.payload);
                None
            }
            TYPE_LAST if self.active => {
                self.buffer.extend_from_slice(&frame.payload);
                self.active = false;
                Some(std::mem::take(&mut self.buffer))
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_round_trip() {
        let open = open_session(0x1234_5678);
        assert_eq!(open[0], TYPE_SESSION);
        // Device answers with a different sequence byte and its own version.
        let mut reply = vec![TYPE_SESSION, 0x42];
        reply.extend_from_slice(&0x1234_5678u32.to_le_bytes());
        reply.extend_from_slice(&[2, 0, 22]);
        reply.extend_from_slice(&SESSION_MAX_MESSAGE.to_le_bytes());
        let accept = SessionAccept::parse(&reply, 0x1234_5678, 0).unwrap();
        assert_eq!(accept.handle, 0x42);
        assert_eq!(accept.max_message, SESSION_MAX_MESSAGE);
    }

    #[test]
    fn duplicate_reply_is_ignored() {
        let mut reply = vec![TYPE_SESSION, 0];
        reply.extend_from_slice(&1u32.to_le_bytes());
        reply.extend_from_slice(&[2, 0, 22]);
        reply.extend_from_slice(&[0, 0x10]);
        assert!(SessionAccept::parse(&reply, 1, 0).is_none());
    }

    #[test]
    fn single_frame_message_round_trips() {
        let mut seq = Sequence::default();
        let frames = fragments(0x42, 0x2e, &mut seq, &[1, 2, 3], 57);
        assert_eq!(frames.len(), 1);
        let frame = Frame::parse(&frames[0]).unwrap();
        assert_eq!(frame.kind, TYPE_SINGLE);
        let mut reassembler = Reassembler::default();
        assert_eq!(reassembler.push(&frame), Some(vec![1, 2, 3]));
    }

    #[test]
    fn fragmented_message_round_trips() {
        let message: Vec<u8> = (0..200u8).collect();
        let mut seq = Sequence::default();
        let frames = fragments(0x42, 0x03, &mut seq, &message, 57);
        assert_eq!(frames.len(), 4);
        let mut reassembler = Reassembler::default();
        let mut out = None;
        for (index, bytes) in frames.iter().enumerate() {
            let frame = Frame::parse(bytes).unwrap();
            assert_eq!(frame.seq, index as u8);
            out = reassembler.push(&frame);
        }
        assert_eq!(out, Some(message));
    }

    #[test]
    fn ack_encodes_next_sequence() {
        assert_eq!(ack(0x42, 0x03, 7), vec![TYPE_ACK, 0x42, 0x03, 8]);
    }
}
