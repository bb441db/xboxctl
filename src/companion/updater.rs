//! Host side of the Companion Protocol: session handshake, device info, and
//! the firmware update sequence (status, start, write, finish).

use crate::companion::firmware::Image;
use crate::companion::frame::{self, Frame, Reassembler, Sequence, SessionAccept};
use crate::companion::protocol::{
    self, DeviceInfo, LegacyDeviceInfo, StartReply, Status, UpdateStatus, WriteReply,
};
use std::time::{Duration, Instant};

/// A packet transport to one Pendragon device's companion HID collection.
/// `send`/`recv` carry one HID report's payload, i.e. everything after the
/// `0x0c` report ID.
pub trait Link {
    fn send(&mut self, payload: &[u8]) -> Result<(), String>;
    /// Next device report, or `None` after `timeout`.
    fn recv(&mut self, timeout: Duration) -> Result<Option<Vec<u8>>, String>;
    /// Frame payload bytes one report can carry (its length minus the data
    /// frame header). Depends on the report size, which is not known to be
    /// the same on every Pendragon device.
    fn max_fragment_data(&self) -> usize;
}

#[derive(Debug, Clone)]
pub struct Options {
    pub session_timeout: Duration,
    pub response_timeout: Duration,
    /// SDK timeout for start and finish commands.
    pub mutation_timeout: Duration,
    pub write_timeout: Duration,
    pub ack_timeout: Duration,
    pub attempts: u32,
    /// Matches `UpdateDeviceFirmwareAsync(foreground: true)`, which is the
    /// only path proven to work without `XboxAccessoriesService` running.
    pub foreground: bool,
    /// Finish flags: the SDK uses 3 for a feed-mandated update, 1 for a
    /// normal foreground update.
    pub finish_flags: u8,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            session_timeout: Duration::from_secs(5),
            response_timeout: Duration::from_secs(4),
            mutation_timeout: Duration::from_secs(20),
            write_timeout: Duration::from_secs(100),
            ack_timeout: Duration::from_millis(500),
            attempts: 5,
            foreground: true,
            finish_flags: 1,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    UpToDate,
    Begin { resume_offset: u32, total: u32 },
    Progress { done: u32, total: u32 },
    Finished,
}

/// A device's answer to a device-info request: the current full format, or
/// the shorter legacy one an older negotiated protocol falls back to (no
/// capabilities or firmware version, see [`LegacyDeviceInfo`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceInfoReply {
    Full(DeviceInfo),
    Legacy(LegacyDeviceInfo),
}

pub struct Session<'a, L: Link> {
    link: &'a mut L,
    options: Options,
    handle: u8,
    /// Smallest of what the host offered and what the device accepted.
    max_message: u16,
    version: [u8; 3],
    seq: Sequence,
    last_rx: Option<u8>,
}

impl<'a, L: Link> Session<'a, L> {
    /// Open a session: send the control packet and wait for the device's
    /// accept. `session_id` should be unpredictable so a stale reply from a
    /// previous session cannot be mistaken for this one.
    pub fn open(link: &'a mut L, session_id: u32, options: Options) -> Result<Self, String> {
        let deadline = Instant::now() + options.session_timeout;
        let attempts = options.attempts.max(1);
        let interval = options.session_timeout / attempts;
        for _ in 0..attempts {
            if Instant::now() >= deadline {
                break;
            }
            link.send(&frame::open_session(session_id))?;
            let reply_deadline = (Instant::now() + interval).min(deadline);
            loop {
                let remaining = reply_deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    break;
                }
                let Some(bytes) = link.recv(remaining)? else {
                    break;
                };
                if let Some(accept) = SessionAccept::parse(&bytes, session_id, 0) {
                    return Ok(Self {
                        link,
                        max_message: accept.max_message.min(frame::SESSION_MAX_MESSAGE),
                        handle: accept.handle,
                        version: accept.version,
                        seq: Sequence::default(),
                        last_rx: None,
                        options,
                    });
                }
            }
        }
        Err(format!(
            "device did not accept a session after {attempts} attempts"
        ))
    }

    pub fn set_finish_flags(&mut self, flags: u8) {
        self.options.finish_flags = flags;
    }

    /// Send one message as one or more data frames, waiting for the device's
    /// ack after every fragment (stop-and-wait, as the protocol requires). A
    /// message that fits one report needs no ack.
    fn send_message(&mut self, opcode: u8, payload: &[u8]) -> Result<(), String> {
        let max_data = self.link.max_fragment_data();
        if max_data == 0 {
            return Err("report too small for the companion protocol".to_owned());
        }
        for report in frame::fragments(self.handle, opcode, &mut self.seq, payload, max_data) {
            let sent = Frame::parse(&report).expect("fragments() produces valid frames");
            if !sent.is_fragment() {
                self.link.send(&report)?;
                continue;
            }
            let sends = self.options.attempts.max(1);
            let mut acked = false;
            for _ in 0..sends {
                self.link.send(&report)?;
                let deadline = Instant::now() + self.options.ack_timeout;
                while !acked {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        break;
                    }
                    let Some(bytes) = self.link.recv(remaining)? else {
                        break;
                    };
                    let Some(reply) = Frame::parse(&bytes) else {
                        continue;
                    };
                    if reply.kind == frame::TYPE_ACK
                        && reply.handle == self.handle
                        && reply.opcode == opcode
                        && reply.seq == sent.seq.wrapping_add(1)
                    {
                        acked = true;
                        break;
                    }
                    // Other device traffic can arrive while a write fragment
                    // is awaiting its ack. Ack it so it cannot clog the HID
                    // queue; retry the exact same fragment/sequence below.
                    if reply.kind != frame::TYPE_ACK {
                        self.link
                            .send(&frame::ack(reply.handle, reply.opcode, reply.seq))?;
                    }
                }
                if acked {
                    break;
                }
            }
            if !acked {
                return Err(format!(
                    "no ack for message fragment {:#04x} after {sends} sends",
                    sent.seq
                ));
            }
        }
        Ok(())
    }

    /// Send a request and return the reassembled reply, acking any fragments
    /// of it as they arrive. Retries the whole exchange on timeout.
    fn request(
        &mut self,
        opcode: u8,
        payload: &[u8],
        timeout: Duration,
    ) -> Result<Vec<u8>, String> {
        self.request_with_attempts(opcode, payload, timeout, self.options.attempts)
    }

    fn request_with_attempts(
        &mut self,
        opcode: u8,
        payload: &[u8],
        timeout: Duration,
        attempts: u32,
    ) -> Result<Vec<u8>, String> {
        for _ in 0..attempts {
            self.send_message(opcode, payload)?;
            let deadline = Instant::now() + timeout;
            let mut reassembler = Reassembler::default();
            let mut expected = None;
            loop {
                let remaining = deadline.saturating_duration_since(Instant::now());
                let Some(bytes) = self.link.recv(remaining)? else {
                    break;
                };
                let Some(frame) = Frame::parse(&bytes) else {
                    continue;
                };
                if frame.kind == frame::TYPE_ACK {
                    continue;
                }
                // A real Nano expects an ack for every data frame it sends,
                // not only fragmented ones: a single-frame reply that goes
                // unacked is retransmitted forever, confirmed 2026-11-14
                // against physical hardware (see research/nano/README.md).
                self.link
                    .send(&frame::ack(frame.handle, frame.opcode, frame.seq))?;
                if frame.handle != self.handle || frame.opcode != opcode {
                    continue;
                }
                if matches!(frame.kind, frame::TYPE_SINGLE | frame::TYPE_FIRST) {
                    if self
                        .last_rx
                        .is_some_and(|last| frame.seq.wrapping_sub(last).wrapping_sub(1) >= 0x80)
                    {
                        continue;
                    }
                    self.last_rx = Some(frame.seq);
                } else if expected != Some(frame.seq) {
                    continue;
                }
                expected = Some(frame.seq.wrapping_add(1));
                if let Some(message) = reassembler.push(&frame) {
                    return Ok(message);
                }
            }
        }
        Err(format!(
            "no response to opcode {opcode:#04x} after {attempts} attempts"
        ))
    }

    /// Select the device-info opcode from the version negotiated at session
    /// open. Older protocol versions use `0x23`, with `0x1c` as a fallback.
    pub fn device_info(&mut self) -> Result<DeviceInfoReply, String> {
        let payload = protocol::device_info_request();
        let opcode = protocol::device_info_opcode(self.version);
        let response = match self.request(opcode, &payload, self.options.response_timeout) {
            Ok(response) => response,
            Err(_) if opcode == protocol::OP_DEVICE_INFO_LEGACY => self.request(
                protocol::OP_DEVICE_INFO_OLD_1,
                &payload,
                self.options.response_timeout,
            )?,
            Err(error) => return Err(error),
        };
        if opcode == protocol::OP_DEVICE_INFO_LEGACY {
            LegacyDeviceInfo::parse(&response)
                .map(DeviceInfoReply::Legacy)
                .ok_or_else(|| "malformed legacy device information reply".to_owned())
        } else {
            DeviceInfo::parse(&response)
                .map(DeviceInfoReply::Full)
                .ok_or_else(|| "malformed device information reply".to_owned())
        }
    }

    /// `03`/`14`: read-only (no write, no state change on the device) even
    /// though it lives under the update opcode; callers use it to learn the
    /// running firmware version when the legacy device-info format carries no
    /// version.
    pub fn update_status(&mut self) -> Result<UpdateStatus, String> {
        let timeout = self.options.response_timeout;
        let response = self.request(protocol::OP_UPDATE, &protocol::status_request(), timeout)?;
        UpdateStatus::parse(&response).ok_or_else(|| "malformed update status reply".to_owned())
    }

    pub(crate) fn start_update(
        &mut self,
        status: &UpdateStatus,
        image: &Image,
    ) -> Result<StartReply, String> {
        let timeout = self.options.mutation_timeout;
        let request = protocol::start_request(
            status.token,
            image.version,
            self.options.foreground,
            status.mode,
        );
        let response = self.request_with_attempts(protocol::OP_UPDATE, &request, timeout, 1)?;
        StartReply::parse(&response).ok_or_else(|| "malformed update start reply".to_owned())
    }

    fn write_block(&mut self, slot: u8, offset: u32, data: &[u8]) -> Result<WriteReply, String> {
        let timeout = self.options.write_timeout;
        let request = protocol::write_request(slot, offset, data);
        let response = self.request_with_attempts(protocol::OP_UPDATE, &request, timeout, 1)?;
        WriteReply::parse_for_version(&response, self.version)
            .ok_or_else(|| "malformed write reply".to_owned())
    }

    fn finish_update(&mut self, slot: u8) -> Result<Status, String> {
        let timeout = self.options.mutation_timeout;
        let request = protocol::finish_request_with_flags(slot, self.options.finish_flags);
        let response = self.request_with_attempts(protocol::OP_UPDATE, &request, timeout, 1)?;
        protocol::finish_status(&response).ok_or_else(|| "malformed finish reply".to_owned())
    }

    /// Run the firmware update sequence: status, start, write the image from
    /// the device's resume offset, finish. The device is expected to restart
    /// into the new firmware by itself a second or two after finishing; this
    /// does not wait for that.
    pub fn update(&mut self, image: &Image, events: &mut dyn FnMut(&Event)) -> Result<(), String> {
        if image.data.len() > 0x00ff_ffff {
            return Err(
                "firmware image exceeds the 24-bit transfer limit; not starting".to_owned(),
            );
        }
        if self.max_message <= 5 {
            return Err(
                "negotiated message size cannot hold a firmware block; not starting".to_owned(),
            );
        }
        let status = self.update_status()?;
        if !matches!(status.status, Status::Ok | Status::NotUpdating) {
            return Err(format!("device reported update status {:?}", status.status));
        }
        if status.running_version == image.version {
            events(&Event::UpToDate);
            return Ok(());
        }
        if status.mode == 5 {
            return Err(
                "device is waiting for update consent (mode 5); this is not implemented".to_owned(),
            );
        }
        if status.max_chunk == 0 {
            return Err("device reports a zero maximum write size; not starting".to_owned());
        }

        let start = self.start_update(&status, image)?;
        if start.status != Status::Ok {
            return Err(format!("update start failed: {:?}", start.status));
        }
        let total = image.data.len() as u32;
        if start.resume_offset > total {
            return Err(format!(
                "resume offset {} exceeds image size {total}; not writing or finishing",
                start.resume_offset
            ));
        }
        events(&Event::Begin {
            resume_offset: start.resume_offset,
            total,
        });

        let chunk = (usize::from(self.max_message) - 5).min(usize::from(status.max_chunk));
        let mut offset = start.resume_offset as usize;
        while offset < image.data.len() {
            let end = (offset + chunk).min(image.data.len());
            let reply = self.write_block(start.slot, offset as u32, &image.data[offset..end])?;
            if reply.status != Status::Ok {
                return Err(format!("write at {offset:#x} failed: {:?}", reply.status));
            }
            offset = end;
            events(&Event::Progress {
                done: offset as u32,
                total,
            });
        }

        let final_status = self.update_status()?;
        if final_status.status != Status::Ok {
            return Err(format!(
                "device reported final update status {:?}; not finishing",
                final_status.status
            ));
        }
        let written = final_status
            .bytes_written
            .ok_or_else(|| "device did not report bytes written; not finishing".to_owned())?;
        if written != total {
            return Err(format!(
                "device reports {written} of {total} bytes written; not finishing"
            ));
        }

        let status = self.finish_update(start.slot)?;
        if status != Status::Ok {
            return Err(format!("finish failed: {status:?}"));
        }
        events(&Event::Finished);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::companion::device::{CompanionEndpoint, FakeNano};
    use std::collections::VecDeque;

    struct MockLink {
        endpoint: CompanionEndpoint,
        queue: VecDeque<Vec<u8>>,
    }

    impl MockLink {
        fn new(firmware_version: [u16; 4]) -> Self {
            Self {
                endpoint: CompanionEndpoint::new(FakeNano::tala(firmware_version)),
                queue: VecDeque::new(),
            }
        }
    }

    impl Link for MockLink {
        fn send(&mut self, payload: &[u8]) -> Result<(), String> {
            self.queue.extend(self.endpoint.handle_report(payload));
            Ok(())
        }

        fn recv(&mut self, _timeout: Duration) -> Result<Option<Vec<u8>>, String> {
            Ok(self.queue.pop_front())
        }

        fn max_fragment_data(&self) -> usize {
            57
        }
    }

    fn open(link: &mut MockLink) -> Session<'_, MockLink> {
        Session::open(link, 0x1234_5678, Options::default()).unwrap()
    }

    #[test]
    fn reads_fragmented_device_info() {
        let mut link = MockLink::new([6, 19, 49, 0]);
        let mut session = open(&mut link);
        let DeviceInfoReply::Full(info) = session.device_info().unwrap() else {
            panic!("expected the full device info format");
        };
        assert_eq!((info.vendor_id, info.product_id), (0x045e, 0x0b4c));
        assert_eq!(info.hardware_revision, 0x0c06);
        assert_eq!(info.firmware_version, [6, 19, 49, 0]);
        assert!(info.is_updatable());
    }

    #[test]
    fn updates_to_the_image_version() {
        let mut link = MockLink::new([6, 19, 49, 0]);
        let image = Image {
            version: [6, 19, 50, 0],
            data: (0..0x2000u32).map(|i| i as u8).collect(),
        };
        let mut events = Vec::new();
        let mut record = |event: &Event| events.push(event.clone());
        {
            let mut session = open(&mut link);
            session.update(&image, &mut record).unwrap();
        }
        assert!(matches!(
            events[0],
            Event::Begin {
                resume_offset: 0,
                ..
            }
        ));
        assert!(events.contains(&Event::Finished));
        assert_eq!(link.endpoint.device.written_data(), &image.data[..]);
        assert_eq!(link.endpoint.device.firmware_version, image.version);
    }

    #[test]
    fn already_up_to_date_writes_nothing() {
        let mut link = MockLink::new([6, 19, 50, 0]);
        let image = Image {
            version: [6, 19, 50, 0],
            data: vec![0xaa; 0x100],
        };
        let mut events = Vec::new();
        let mut record = |event: &Event| events.push(event.clone());
        let mut session = open(&mut link);
        session.update(&image, &mut record).unwrap();
        assert_eq!(events, vec![Event::UpToDate]);
        assert!(link.endpoint.device.written_data().is_empty());
    }
}
