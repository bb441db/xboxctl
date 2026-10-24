//! Stick calibration (command `0x1e`), as sent by the Xbox Accessories app.
//!
//! The layout below is from issue #13, which captured it from the app and
//! tried it on a Series X|S controller. The controller only answers on
//! firmware 5.19.3221.0 or later.
//!
//! The Xbox Accessories app has just these two operations (`StartCalibration`
//! and `GetCalibration` of its `ICalibrationApi`), so the controller decides
//! itself when a calibration is finished.

use super::frame::{self, Packet};

pub const CMD_CALIBRATION: u8 = 0x1e;

/// Sub-command: read the current limits of the selected controls.
const READ: u8 = 0x0f;
/// Sub-command: make the current stick position the new centre.
const RECENTER: u8 = 0x10;

/// Which controls a request applies to (bit 0 left stick, bit 1 right stick).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sticks {
    pub left: bool,
    pub right: bool,
}

impl Sticks {
    pub const BOTH: Self = Self {
        left: true,
        right: true,
    };

    pub fn mask(self) -> u8 {
        u8::from(self.left) | u8::from(self.right) << 1
    }
}

/// Request that reports the calibration of the selected sticks.
pub fn read_packet(sequence: u8, sticks: Sticks) -> Vec<u8> {
    request(sequence, READ, sticks)
}

/// Request that recenters the selected sticks at their current position. The
/// controller stores the result; keep the sticks at rest while it runs.
pub fn recenter_packet(sequence: u8, sticks: Sticks) -> Vec<u8> {
    request(sequence, RECENTER, sticks)
}

fn request(sequence: u8, sub_command: u8, sticks: Sticks) -> Vec<u8> {
    frame::packet(
        CMD_CALIBRATION,
        frame::FLAG_ACME,
        sequence,
        &[sub_command, sticks.mask()],
    )
}

/// Command of the input report.
pub const CMD_INPUT: u8 = 0x20;

/// Stick positions of one input report (signed 16-bit, 0 at rest).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Axes {
    pub left_x: i16,
    pub left_y: i16,
    pub right_x: i16,
    pub right_y: i16,
}

impl Axes {
    /// Input report payload: buttons u16, both triggers u16, then the four
    /// axes as little-endian i16 (left X, left Y, right X, right Y).
    pub fn parse(packet: &Packet) -> Option<Self> {
        let p = packet.payload.as_slice();
        if packet.command != CMD_INPUT || p.len() < 14 {
            return None;
        }
        let axis = |i: usize| i16::from_le_bytes([p[i], p[i + 1]]);
        Some(Self {
            left_x: axis(6),
            left_y: axis(8),
            right_x: axis(10),
            right_y: axis(12),
        })
    }
}

/// Raw axis limits of one stick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub horizontal_min: u16,
    pub horizontal_max: u16,
    pub vertical_min: u16,
    pub vertical_max: u16,
}

/// A controller reply to a calibration request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Response {
    /// Reply to a read: `0f 00 <status>`, then one block per stick: a mask
    /// byte (1 left stick, 2 right stick) and four little-endian u16 values,
    /// the (min, max) of the horizontal axis and then of the vertical axis.
    ///
    /// These are the stored calibration of that stick (firmware NV record
    /// 0x11): raw 12-bit readings, with the centre near 0x800 on a captured
    /// controller. Zeros mean "not calibrated", and the controller then uses
    /// built-in defaults. A recenter leaves them at zero (see `Recenter`).
    /// `status` is 0 when idle. Observed on firmware 5.23.6.0 after a
    /// recenter of the left stick: 1 while it waits for rotations, then 0x10
    /// for about half a second once the limits are filled in, then 0. It
    /// finished after about 6 clockwise and 5 counter-clockwise rotations,
    /// without the up/right/down/left pushes of the wizard. Starting a
    /// recenter while the status was not 0 never finished.
    Read {
        status: u8,
        left: Option<Limits>,
        right: Option<Limits>,
    },
    /// Reply to a recenter: `10 <status> <mask>`, echoing the mask that was
    /// sent. It only starts the calibration: the limits read as zero until the
    /// stick has been rotated through its range (see `Read`).
    Recenter { status: u8, mask: u8 },
}

impl Response {
    pub fn parse(packet: &Packet) -> Option<Self> {
        if packet.command != CMD_CALIBRATION {
            return None;
        }
        let payload = packet.payload.as_slice();
        match *payload.first()? {
            READ => {
                let status = *payload.get(2)?;
                let (mut left, mut right) = (None, None);
                for block in payload.get(3..)?.chunks(9) {
                    let [mask, data @ ..] = block else {
                        unreachable!("chunks are never empty");
                    };
                    let data: &[u8; 8] = data.try_into().ok()?;
                    let word = |i: usize| u16::from_le_bytes([data[i * 2], data[i * 2 + 1]]);
                    let limits = Limits {
                        horizontal_min: word(0),
                        horizontal_max: word(1),
                        vertical_min: word(2),
                        vertical_max: word(3),
                    };
                    match mask {
                        1 => left = Some(limits),
                        2 => right = Some(limits),
                        _ => {}
                    }
                }
                Some(Self::Read {
                    status,
                    left,
                    right,
                })
            }
            RECENTER => Some(Self::Recenter {
                status: *payload.get(1)?,
                mask: *payload.get(2)?,
            }),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply(payload: &[u8]) -> Packet {
        Packet::parse(&frame::packet(
            CMD_CALIBRATION,
            frame::FLAG_ACME,
            7,
            payload,
        ))
        .unwrap()
    }

    #[test]
    fn requests_carry_the_sub_command_and_mask() {
        assert_eq!(
            read_packet(3, Sticks::BOTH),
            [0x1e, 0x10, 0x03, 0x02, 0x0f, 0x03]
        );
        assert_eq!(
            recenter_packet(
                4,
                Sticks {
                    left: true,
                    right: false
                }
            ),
            [0x1e, 0x10, 0x04, 0x02, 0x10, 0x01]
        );
        assert_eq!(
            read_packet(
                5,
                Sticks {
                    left: false,
                    right: true
                }
            )[5],
            0x02
        );
    }

    /// Captured from a Series X|S controller on firmware 5.23.6.0.
    const CAPTURED_READ: [u8; 21] = [
        0x0f, 0x00, 0x00, 0x01, 0xa6, 0x03, 0x80, 0x0c, 0xba, 0x03, 0x57, 0x0c, 0x02, 0xa1, 0x03,
        0x90, 0x0c, 0x94, 0x03, 0x9c, 0x0c,
    ];

    #[test]
    fn parses_the_captured_read_of_both_sticks() {
        let Some(Response::Read {
            status,
            left,
            right,
        }) = Response::parse(&reply(&CAPTURED_READ))
        else {
            panic!("not a read response");
        };
        assert_eq!(status, 0);
        assert_eq!(
            left,
            Some(Limits {
                horizontal_min: 0x03a6,
                horizontal_max: 0x0c80,
                vertical_min: 0x03ba,
                vertical_max: 0x0c57,
            })
        );
        assert_eq!(
            right,
            Some(Limits {
                horizontal_min: 0x03a1,
                horizontal_max: 0x0c90,
                vertical_min: 0x0394,
                vertical_max: 0x0c9c,
            })
        );
    }

    #[test]
    fn a_single_stick_read_has_one_block() {
        let Some(Response::Read { left, right, .. }) =
            Response::parse(&reply(&CAPTURED_READ[..12]))
        else {
            panic!("not a read response");
        };
        assert_eq!(left.unwrap().vertical_max, 0x0c57);
        assert_eq!(right, None);
    }

    #[test]
    fn a_truncated_block_is_rejected() {
        assert_eq!(Response::parse(&reply(&CAPTURED_READ[..16])), None);
    }

    #[test]
    fn parses_the_captured_recenter_reply() {
        assert_eq!(
            Response::parse(&reply(&[0x10, 0x00, 0x03])),
            Some(Response::Recenter { status: 0, mask: 3 })
        );
        assert_eq!(Response::parse(&reply(&[0x10, 0x00])), None);
    }

    #[test]
    fn parses_stick_positions_from_an_input_report() {
        let mut payload = vec![0u8; 6];
        for value in [-300i16, 25, 32767, -32768] {
            payload.extend_from_slice(&value.to_le_bytes());
        }
        let packet = Packet::parse(&frame::packet(CMD_INPUT, 0, 9, &payload)).unwrap();
        assert_eq!(
            Axes::parse(&packet),
            Some(Axes {
                left_x: -300,
                left_y: 25,
                right_x: 32767,
                right_y: -32768,
            })
        );
        assert_eq!(Axes::parse(&reply(&payload)), None);
    }

    #[test]
    fn other_commands_are_not_calibration() {
        let other = Packet::parse(&frame::packet(0x20, 0, 1, &CAPTURED_READ)).unwrap();
        assert_eq!(Response::parse(&other), None);
    }
}
