//! Stick and trigger calibration (command `0x1e`), as sent by the Xbox Accessories app.
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
/// Sub-command: start a calibration of the selected controls.
const RECENTER: u8 = 0x10;

/// Which controls a request applies to: a mask of bit 0 left stick, bit 1
/// right stick, bit 2 left trigger, bit 3 right trigger.
///
/// The stick bits are observed. The trigger bits are an inference from the
/// firmware (its trigger recenter looks at bits 2 and 3 of the same mask) and
/// have not been checked against a controller yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Controls(pub u8);

impl Controls {
    pub const LEFT_STICK: Self = Self(0x01);
    pub const RIGHT_STICK: Self = Self(0x02);
    pub const LEFT_TRIGGER: Self = Self(0x04);
    pub const RIGHT_TRIGGER: Self = Self(0x08);
    pub const BOTH_STICKS: Self = Self(0x03);
    pub const BOTH_TRIGGERS: Self = Self(0x0c);

    pub fn mask(self) -> u8 {
        self.0
    }
}

/// Request that reports the calibration of the selected controls.
pub fn read_packet(sequence: u8, controls: Controls) -> Vec<u8> {
    request(sequence, READ, controls)
}

/// Request that recenters the selected controls at their current position. The
/// controller stores the result; keep the sticks at rest while it runs.
pub fn recenter_packet(sequence: u8, controls: Controls) -> Vec<u8> {
    request(sequence, RECENTER, controls)
}

fn request(sequence: u8, sub_command: u8, controls: Controls) -> Vec<u8> {
    frame::packet(
        CMD_CALIBRATION,
        frame::FLAG_ACME,
        sequence,
        &[sub_command, controls.mask()],
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

/// Trigger positions of one input report (0 at rest, `Triggers::MAX` fully
/// pressed; the firmware treats 0x3ff as a full press).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Triggers {
    pub left: u16,
    pub right: u16,
}

impl Triggers {
    pub const MAX: u16 = 0x3ff;

    /// Input report payload: buttons u16, then the left and right trigger as
    /// little-endian u16.
    pub fn parse(packet: &Packet) -> Option<Self> {
        let p = packet.payload.as_slice();
        if packet.command != CMD_INPUT || p.len() < 6 {
            return None;
        }
        Some(Self {
            left: u16::from_le_bytes([p[2], p[3]]),
            right: u16::from_le_bytes([p[4], p[5]]),
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

/// Raw reading limits of one trigger.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TriggerLimits {
    /// Byte before the limits; 2 on the controller it was seen on.
    pub flag: u8,
    pub min: u16,
    pub max: u16,
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
    ///
    /// A trigger (mask 4 left, 8 right) has a block of 6 bytes: the mask, a
    /// flag byte (2 on both triggers of a Series X|S on 5.23.6.0, meaning
    /// unknown), and the little-endian u16 (min, max) of its raw reading.
    Read {
        status: u8,
        left: Option<Limits>,
        right: Option<Limits>,
        left_trigger: Option<TriggerLimits>,
        right_trigger: Option<TriggerLimits>,
    },
    /// Reply to a recenter: `10 <status> <mask>`, echoing the mask that was
    /// sent. It only starts the calibration: the limits read as zero until the
    /// stick has been rotated through its range (see `Read`).
    Recenter { status: u8, mask: u8 },
    /// Reply of two bytes, `<sub-command> <code>`: the controller understood
    /// the request and refused it. An Xbox One S controller on firmware 5.22
    /// answered every read with `0f 01`.
    Rejected { sub_command: u8, code: u8 },
}

impl Response {
    pub fn parse(packet: &Packet) -> Option<Self> {
        if packet.command != CMD_CALIBRATION {
            return None;
        }
        let payload = packet.payload.as_slice();
        if let &[sub_command @ (READ | RECENTER), code] = payload
            && code != 0
        {
            return Some(Self::Rejected { sub_command, code });
        }
        match *payload.first()? {
            READ => {
                let status = *payload.get(2)?;
                let (mut left, mut right) = (None, None);
                let (mut left_trigger, mut right_trigger) = (None, None);
                // Blocks differ in size by what they describe: a stick is 9
                // bytes, a trigger 6.
                let mut rest = payload.get(3..)?;
                while let Some((&mask, data)) = rest.split_first() {
                    let size = match mask {
                        1 | 2 => 8,
                        4 | 8 => 5,
                        _ => break,
                    };
                    let (data, tail) = data.split_at_checked(size)?;
                    rest = tail;
                    let word = |i: usize| u16::from_le_bytes([data[i * 2], data[i * 2 + 1]]);
                    match mask {
                        1 | 2 => {
                            let limits = Limits {
                                horizontal_min: word(0),
                                horizontal_max: word(1),
                                vertical_min: word(2),
                                vertical_max: word(3),
                            };
                            if mask == 1 {
                                left = Some(limits);
                            } else {
                                right = Some(limits);
                            }
                        }
                        _ => {
                            let limits = TriggerLimits {
                                flag: data[0],
                                min: u16::from_le_bytes([data[1], data[2]]),
                                max: u16::from_le_bytes([data[3], data[4]]),
                            };
                            if mask == 4 {
                                left_trigger = Some(limits);
                            } else {
                                right_trigger = Some(limits);
                            }
                        }
                    }
                }
                Some(Self::Read {
                    status,
                    left,
                    right,
                    left_trigger,
                    right_trigger,
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
            read_packet(3, Controls::BOTH_STICKS),
            [0x1e, 0x10, 0x03, 0x02, 0x0f, 0x03]
        );
        assert_eq!(
            recenter_packet(4, Controls::LEFT_STICK),
            [0x1e, 0x10, 0x04, 0x02, 0x10, 0x01]
        );
        assert_eq!(read_packet(5, Controls::RIGHT_STICK)[5], 0x02);
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
            ..
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

    /// Both triggers of a Series X|S controller on firmware 5.23.6.0.
    const CAPTURED_TRIGGERS: [u8; 15] = [
        0x0f, 0x00, 0x00, 0x04, 0x02, 0x6d, 0x04, 0xdf, 0x09, 0x08, 0x02, 0xbb, 0x04, 0xfc, 0x09,
    ];

    #[test]
    fn parses_the_captured_read_of_both_triggers() {
        let Some(Response::Read {
            status,
            left,
            left_trigger,
            right_trigger,
            ..
        }) = Response::parse(&reply(&CAPTURED_TRIGGERS))
        else {
            panic!("not a read response");
        };
        assert_eq!(status, 0);
        assert_eq!(left, None);
        assert_eq!(
            left_trigger,
            Some(TriggerLimits {
                flag: 2,
                min: 0x046d,
                max: 0x09df
            })
        );
        assert_eq!(
            right_trigger,
            Some(TriggerLimits {
                flag: 2,
                min: 0x04bb,
                max: 0x09fc
            })
        );
    }

    #[test]
    fn a_single_trigger_read_has_one_block() {
        let Some(Response::Read {
            left_trigger,
            right_trigger,
            ..
        }) = Response::parse(&reply(&CAPTURED_TRIGGERS[..9]))
        else {
            panic!("not a read response");
        };
        assert_eq!(left_trigger.unwrap().max, 0x09df);
        assert_eq!(right_trigger, None);
        assert_eq!(Response::parse(&reply(&CAPTURED_TRIGGERS[..12])), None);
    }

    #[test]
    fn sticks_and_triggers_can_share_a_reply() {
        let mut payload = CAPTURED_READ[..12].to_vec();
        payload.extend_from_slice(&CAPTURED_TRIGGERS[3..9]);
        let Some(Response::Read {
            left,
            left_trigger,
            right,
            ..
        }) = Response::parse(&reply(&payload))
        else {
            panic!("not a read response");
        };
        assert_eq!(left.unwrap().vertical_max, 0x0c57);
        assert_eq!(right, None);
        assert_eq!(left_trigger.unwrap().min, 0x046d);
    }

    #[test]
    fn parses_a_rejection() {
        // An Xbox One S controller (CRET) on firmware 5.22.
        assert_eq!(
            Response::parse(&reply(&[0x0f, 0x01])),
            Some(Response::Rejected {
                sub_command: 0x0f,
                code: 1
            })
        );
        assert_eq!(Response::parse(&reply(&[0x42, 0x01])), None);
        // A code of zero is not a refusal.
        assert_eq!(Response::parse(&reply(&[0x0f, 0x00])), None);
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
    fn parses_trigger_positions_from_an_input_report() {
        let mut payload = vec![0u8, 0];
        for value in [0u16, 1023] {
            payload.extend_from_slice(&value.to_le_bytes());
        }
        let packet = Packet::parse(&frame::packet(CMD_INPUT, 0, 9, &payload)).unwrap();
        assert_eq!(
            Triggers::parse(&packet),
            Some(Triggers {
                left: 0,
                right: 1023
            })
        );
        assert_eq!(Triggers::parse(&reply(&payload)), None);
    }

    #[test]
    fn other_commands_are_not_calibration() {
        let other = Packet::parse(&frame::packet(0x20, 0, 1, &CAPTURED_READ)).unwrap();
        assert_eq!(Response::parse(&other), None);
    }
}
