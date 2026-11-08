//! Host side of the Companion Protocol: session handshake, device info, and
//! the firmware update sequence (status, start, write, finish).

use crate::companion::firmware::Image;
use crate::companion::frame::{self, Frame, Reassembler, Sequence, SessionAccept};
use crate::companion::protocol::{self, DeviceInfo, StartReply, Status, UpdateStatus, WriteReply};
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
    pub write_timeout: Duration,
    pub ack_timeout: Duration,
    pub attempts: u32,
    /// Matches `UpdateDeviceFirmwareAsync(foreground: true)`, which is the
    /// only path proven to work without `XboxAccessoriesService` running.
    pub foreground: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            session_timeout: Duration::from_secs(5),
            response_timeout: Duration::from_secs(4),
            write_timeout: Duration::from_secs(10),
            ack_timeout: Duration::from_millis(500),
            attempts: 5,
            foreground: true,
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

pub struct Session<'a, L: Link> {
    link: &'a mut L,
    options: Options,
    handle: u8,
    /// Smallest of what the host offered and what the device accepted.
    max_message: u16,
    seq: Sequence,
}

impl<'a, L: Link> Session<'a, L> {
    /// Open a session: send the control packet and wait for the device's
    /// accept. `session_id` should be unpredictable so a stale reply from a
    /// previous session cannot be mistaken for this one.
    pub fn open(link: &'a mut L, session_id: u32, options: Options) -> Result<Self, String> {
        link.send(&frame::open_session(session_id))?;
        let deadline = Instant::now() + options.session_timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let Some(bytes) = link.recv(remaining)? else {
                return Err("device did not accept a session".to_owned());
            };
            if let Some(accept) = SessionAccept::parse(&bytes, session_id, 0) {
                return Ok(Self {
                    link,
                    max_message: accept.max_message.min(frame::SESSION_MAX_MESSAGE),
                    handle: accept.handle,
                    seq: Sequence::default(),
                    options,
                });
            }
        }
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
            self.link.send(&report)?;
            if !sent.is_fragment() {
                continue;
            }
            let deadline = Instant::now() + self.options.ack_timeout;
            loop {
                let remaining = deadline.saturating_duration_since(Instant::now());
                let Some(bytes) = self.link.recv(remaining)? else {
                    return Err("no ack for a message fragment".to_owned());
                };
                let Some(ack) = Frame::parse(&bytes) else {
                    continue;
                };
                if ack.kind == frame::TYPE_ACK
                    && ack.handle == self.handle
                    && ack.opcode == opcode
                    && ack.seq == sent.seq.wrapping_add(1)
                {
                    break;
                }
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
        for _ in 0..self.options.attempts {
            self.send_message(opcode, payload)?;
            let deadline = Instant::now() + timeout;
            let mut reassembler = Reassembler::default();
            loop {
                let remaining = deadline.saturating_duration_since(Instant::now());
                let Some(bytes) = self.link.recv(remaining)? else {
                    break;
                };
                let Some(frame) = Frame::parse(&bytes) else {
                    continue;
                };
                if frame.kind == frame::TYPE_ACK || frame.opcode != opcode {
                    continue;
                }
                if frame.is_fragment() {
                    self.link
                        .send(&frame::ack(self.handle, opcode, frame.seq))?;
                }
                if let Some(message) = reassembler.push(&frame) {
                    return Ok(message);
                }
            }
        }
        Err(format!(
            "no response to opcode {opcode:#04x} after {} attempts",
            self.options.attempts
        ))
    }

    pub fn device_info(&mut self) -> Result<DeviceInfo, String> {
        let response = self.request(
            protocol::OP_DEVICE_INFO,
            &protocol::device_info_request(),
            self.options.response_timeout,
        )?;
        DeviceInfo::parse(&response).ok_or_else(|| "malformed device information reply".to_owned())
    }

    fn update_status(&mut self) -> Result<UpdateStatus, String> {
        let timeout = self.options.response_timeout;
        let response = self.request(protocol::OP_UPDATE, &protocol::status_request(), timeout)?;
        UpdateStatus::parse(&response).ok_or_else(|| "malformed update status reply".to_owned())
    }

    fn start_update(&mut self, status: &UpdateStatus, image: &Image) -> Result<StartReply, String> {
        let timeout = self.options.response_timeout;
        let request = protocol::start_request(
            status.token,
            image.version,
            self.options.foreground,
            status.mode,
        );
        let response = self.request(protocol::OP_UPDATE, &request, timeout)?;
        StartReply::parse(&response).ok_or_else(|| "malformed update start reply".to_owned())
    }

    fn write_block(&mut self, slot: u8, offset: u32, data: &[u8]) -> Result<WriteReply, String> {
        let timeout = self.options.write_timeout;
        let request = protocol::write_request(slot, offset, data);
        let response = self.request(protocol::OP_UPDATE, &request, timeout)?;
        WriteReply::parse(&response).ok_or_else(|| "malformed write reply".to_owned())
    }

    fn finish_update(&mut self, slot: u8) -> Result<Status, String> {
        let timeout = self.options.response_timeout;
        let request = protocol::finish_request(slot, self.options.foreground);
        let response = self.request(protocol::OP_UPDATE, &request, timeout)?;
        protocol::finish_status(&response).ok_or_else(|| "malformed finish reply".to_owned())
    }

    /// Run the firmware update sequence: status, start, write the image from
    /// the device's resume offset, finish. The device is expected to restart
    /// into the new firmware by itself a second or two after finishing; this
    /// does not wait for that.
    pub fn update(&mut self, image: &Image, events: &mut dyn FnMut(&Event)) -> Result<(), String> {
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

        let start = self.start_update(&status, image)?;
        if start.status != Status::Ok {
            return Err(format!("update start failed: {:?}", start.status));
        }
        let total = image.data.len() as u32;
        events(&Event::Begin {
            resume_offset: start.resume_offset,
            total,
        });

        let chunk = (usize::from(self.max_message).saturating_sub(5))
            .min(usize::from(status.max_chunk))
            .max(1);
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
        let info = session.device_info().unwrap();
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
