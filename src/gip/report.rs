//! What a controller reports by itself: the input report (buttons, sticks and
//! triggers) and the Xbox button, which comes separately.
//!
//! The layouts follow the public GIP documentation and the Linux `xone`
//! driver, apart from the share button, whose position was found in a capture. The
//! sticks, triggers and all 15 buttons were checked on a Series X|S controller
//! (see `calibration`).

use super::{calibration::CMD_INPUT, frame::Packet};

/// Button bits of the input report, in report order.
pub const BUTTONS: [(u16, &str); 14] = [
    (1 << 2, "Menu"),
    (1 << 3, "View"),
    (1 << 4, "A"),
    (1 << 5, "B"),
    (1 << 6, "X"),
    (1 << 7, "Y"),
    (1 << 8, "Up"),
    (1 << 9, "Down"),
    (1 << 10, "Left"),
    (1 << 11, "Right"),
    (1 << 12, "LB"),
    (1 << 13, "RB"),
    (1 << 14, "LS"),
    (1 << 15, "RS"),
];

/// Virtual key command: `<state> <key>`, state 1 for down and 0 for up. It is
/// how the Xbox button is reported, as the Windows key. The controller asks for
/// an ACK of each one.
pub const CMD_VIRTUAL_KEY: u8 = 0x07;
const GUIDE_KEY: u8 = 0x5b;

/// Name of the Xbox button.
pub const GUIDE: &str = "Guide";

/// Whether this packet says the Xbox button went down (`Some(true)`) or up.
pub fn guide_button(packet: &Packet) -> Option<bool> {
    match packet.payload.as_slice() {
        [state, GUIDE_KEY, ..] if packet.command == CMD_VIRTUAL_KEY => Some(*state != 0),
        _ => None,
    }
}

/// Payload offset of the share button byte. A Series X|S on firmware 5.23.6.0
/// sends 44 bytes: the 14 above, bytes that stayed zero, this one (0 or 1),
/// more zeros and two 32-bit counters.
const SHARE_BYTE: usize = 18;

/// Name of the Series X|S share button, which is not one of the 16 button bits.
pub const SHARE: &str = "Share";

/// One input report.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct InputReport {
    pub buttons: u16,
    /// The share button: bit 0 of byte 18, when present.
    pub share: bool,
    /// Trigger positions, 0 to 1023.
    pub left_trigger: u16,
    pub right_trigger: u16,
    /// Stick positions, signed 16-bit with 0 at rest and Y up.
    pub left: (i16, i16),
    pub right: (i16, i16),
}

impl InputReport {
    pub fn parse(packet: &Packet) -> Option<Self> {
        let p = packet.payload.as_slice();
        if packet.command != CMD_INPUT || p.len() < 14 {
            return None;
        }
        let word = |i: usize| u16::from_le_bytes([p[i], p[i + 1]]);
        let axis = |i: usize| word(i) as i16;
        Some(Self {
            buttons: word(0),
            share: p.get(SHARE_BYTE).is_some_and(|b| b & 1 != 0),
            left_trigger: word(2),
            right_trigger: word(4),
            left: (axis(6), axis(8)),
            right: (axis(10), axis(12)),
        })
    }

    /// Names of the buttons that are down, in report order.
    pub fn pressed(&self) -> Vec<&'static str> {
        let mut names: Vec<&'static str> = BUTTONS
            .iter()
            .filter(|(bit, _)| self.buttons & bit != 0)
            .map(|&(_, name)| name)
            .collect();
        if self.share {
            names.push(SHARE);
        }
        names
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gip::frame;

    fn packet(command: u8, payload: &[u8]) -> Packet {
        Packet::parse(&frame::packet(command, 0, 1, payload)).unwrap()
    }

    fn input(buttons: u16, triggers: (u16, u16), left: (i16, i16), right: (i16, i16)) -> Vec<u8> {
        let mut payload = buttons.to_le_bytes().to_vec();
        payload.extend(triggers.0.to_le_bytes());
        payload.extend(triggers.1.to_le_bytes());
        for value in [left.0, left.1, right.0, right.1] {
            payload.extend(value.to_le_bytes());
        }
        payload
    }

    #[test]
    fn parses_an_input_report() {
        let payload = input(1 << 4 | 1 << 12, (0, 1023), (-300, 25), (32767, -32768));
        let report = InputReport::parse(&packet(CMD_INPUT, &payload)).unwrap();
        assert_eq!(report.pressed(), ["A", "LB"]);
        assert_eq!((report.left_trigger, report.right_trigger), (0, 1023));
        assert_eq!(report.left, (-300, 25));
        assert_eq!(report.right, (32767, -32768));
        assert!(!report.share);
    }

    /// Captured from a Series X|S controller on firmware 5.23.6.0: Y held,
    /// then the share button.
    const CAPTURED_Y: [u8; 44] = [
        0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x04, 0xb9, 0xfd, 0x86, 0xfc, 0x81, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x35, 0xfc, 0x4c, 0x36,
    ];

    #[test]
    fn parses_the_captured_report() {
        let report = InputReport::parse(&packet(CMD_INPUT, &CAPTURED_Y)).unwrap();
        assert_eq!(report.pressed(), ["Y"]);
        assert_eq!(report.left, (0x0401, -583));
        assert_eq!(report.right, (-890, 129));
    }

    #[test]
    fn the_share_button_is_byte_18_of_the_longer_report() {
        let mut payload = CAPTURED_Y;
        payload[0] = 0;
        payload[18] = 1;
        let report = InputReport::parse(&packet(CMD_INPUT, &payload)).unwrap();
        assert_eq!(report.pressed(), ["Share"]);
        // A shorter report has no share byte, and the old guess is not it.
        let report = InputReport::parse(&packet(CMD_INPUT, &payload[..14])).unwrap();
        assert!(report.pressed().is_empty());
        payload[18] = 0;
        payload[14] = 1;
        let report = InputReport::parse(&packet(CMD_INPUT, &payload)).unwrap();
        assert!(report.pressed().is_empty());
    }

    #[test]
    fn the_xbox_button_is_a_virtual_key() {
        // Captured: `07 30 6b 02 01 5b` and `07 30 6d 02 00 5b`.
        assert_eq!(
            guide_button(&packet(CMD_VIRTUAL_KEY, &[1, 0x5b])),
            Some(true)
        );
        assert_eq!(
            guide_button(&packet(CMD_VIRTUAL_KEY, &[0, 0x5b])),
            Some(false)
        );
        assert_eq!(guide_button(&packet(CMD_VIRTUAL_KEY, &[1, 0x41])), None);
        assert_eq!(guide_button(&packet(CMD_VIRTUAL_KEY, &[1])), None);
        assert_eq!(guide_button(&packet(CMD_INPUT, &[1, 0x5b])), None);
    }

    #[test]
    fn short_or_other_packets_are_not_input_reports() {
        assert_eq!(InputReport::parse(&packet(CMD_INPUT, &[0; 13])), None);
        assert_eq!(InputReport::parse(&packet(0x03, &[0; 20])), None);
    }
}
