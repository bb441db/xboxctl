//! GIP rumble (command `0x09`).

use super::frame::{self, Sequence};

/// Motor enable bits of the rumble payload.
const MOTORS_ALL: u8 = 0x0f;
/// Run until the next rumble message, which can be a stop.
const FOREVER: u8 = 0xff;

/// Motor strengths, 0 (off) to 100.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Motors {
    pub left_trigger: u8,
    pub right_trigger: u8,
    pub left_handle: u8,
    pub right_handle: u8,
}

impl Motors {
    pub const OFF: Self = Self {
        left_trigger: 0,
        right_trigger: 0,
        left_handle: 0,
        right_handle: 0,
    };
}

/// A rumble packet that sets all four motors. It keeps running until the next
/// rumble packet, so end it with `Motors::OFF`. Strengths above 100 are
/// clamped.
pub fn packet(sequence: u8, motors: Motors) -> Vec<u8> {
    let level = |strength: u8| strength.min(100);
    frame::packet(
        frame::CMD_RUMBLE,
        0,
        sequence,
        &[
            0x00,
            MOTORS_ALL,
            level(motors.left_trigger),
            level(motors.right_trigger),
            level(motors.left_handle),
            level(motors.right_handle),
            FOREVER,
            0x00,
            FOREVER,
        ],
    )
}

/// Stop packets with sequence numbers spread over the whole cycle.
///
/// The controller drops rumble whose sequence number is not ahead of the last
/// one it saw, counting with wraparound, and it keeps that number after the
/// host program exits. Sending these first makes sure at least one packet is
/// accepted, whatever the controller remembers. Afterwards `sequence` is just
/// past the last packet, which the controller then accepts, so the rest of the
/// run needs no further care. Send them a few milliseconds apart.
pub fn resync(sequence: &mut Sequence) -> Vec<Vec<u8>> {
    /// Gap between packets; the controller accepts a jump of at least this
    /// much (observed: more than 75).
    const STRIDE: usize = 32;
    let count = 256 / STRIDE;
    (0..count)
        .map(|_| {
            let last = (0..STRIDE).map(|_| sequence.take()).last().unwrap_or(1);
            packet(last, Motors::OFF)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handles_packet_matches_the_captured_one() {
        let motors = Motors {
            left_handle: 0x32,
            right_handle: 0x32,
            ..Motors::OFF
        };
        assert_eq!(
            packet(2, motors),
            [
                0x09, 0x00, 0x02, 0x09, 0x00, 0x0f, 0x00, 0x00, 0x32, 0x32, 0xff, 0x00, 0xff
            ]
        );
    }

    #[test]
    fn triggers_and_stop() {
        let triggers = Motors {
            left_trigger: 0x32,
            right_trigger: 0x32,
            ..Motors::OFF
        };
        assert_eq!(
            packet(3, triggers),
            [
                0x09, 0x00, 0x03, 0x09, 0x00, 0x0f, 0x32, 0x32, 0x00, 0x00, 0xff, 0x00, 0xff
            ]
        );
        assert_eq!(
            packet(4, Motors::OFF),
            [
                0x09, 0x00, 0x04, 0x09, 0x00, 0x0f, 0x00, 0x00, 0x00, 0x00, 0xff, 0x00, 0xff
            ]
        );
    }

    #[test]
    fn strength_is_clamped() {
        let loud = Motors {
            left_handle: 250,
            ..Motors::OFF
        };
        assert_eq!(packet(1, loud)[8], 100);
    }

    #[test]
    fn resync_spreads_stop_packets_around_the_cycle() {
        let mut sequence = Sequence::default();
        let packets = resync(&mut sequence);
        assert_eq!(packets.len(), 8);
        assert!(packets.iter().all(|p| p[0] == 0x09 && p[6..10] == [0; 4]));
        let numbers: Vec<u8> = packets.iter().map(|p| p[2]).collect();
        assert_eq!(numbers[..3], [32, 64, 96]);
        assert!(numbers.iter().all(|&n| n != 0));
        // Each step is a small forward jump, with 0 skipped on the wrap.
        for pair in numbers.windows(2) {
            let step = pair[1].wrapping_sub(pair[0]);
            assert!((32..=34).contains(&step), "{pair:?}");
        }
        // The run continues right after the last packet.
        assert_eq!(sequence.take(), numbers[7].wrapping_add(1).max(1));
    }
}
