//! Host side of UFU: the update sequence the Windows updater runs, as
//! documented in `docs/firmware-update-host.md`.

use crate::frame::{self, Ack, Packet, Sequence};
use crate::rdhm::{Component, Package, Version};
use crate::ufu::{self, ComponentVersion, Info, Status};
use sha1::{Digest, Sha1};
use std::collections::{BTreeMap, VecDeque};
use std::time::{Duration, Instant};

/// A packet transport to one controller's GIP interrupt endpoints.
pub trait Link {
    fn send(&mut self, packet: &[u8]) -> Result<(), String>;
    /// Next device packet, or `None` after `timeout`.
    fn recv(&mut self, timeout: Duration) -> Result<Option<Vec<u8>>, String>;
}

/// Timeouts and retry limits, defaulting to the Windows updater's.
#[derive(Debug, Clone)]
pub struct Options {
    pub response_timeout: Duration,
    pub write_timeout: Duration,
    pub attempts: u32,
    pub ack_timeout: Duration,
    pub reset_timeout: Duration,
    pub iterations: u32,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            response_timeout: Duration::from_secs(4),
            write_timeout: Duration::from_secs(6),
            attempts: 5,
            ack_timeout: Duration::from_millis(500),
            reset_timeout: Duration::from_secs(30),
            iterations: 10,
        }
    }
}

/// Progress reported while updating.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Info(Info),
    DeviceVersion { component: u32, version: Version },
    Order(Vec<u32>),
    UpToDate { component: u32 },
    Begin { component: u32, version: Version, size: u32 },
    Block { component: u32, address: u32, done: u32, total: u32, written: bool },
    Complete { component: u32 },
    Skipped { component: u32, reason: &'static str },
    Reset,
    Retry { op: u8, attempt: u32 },
    FragmentResend { offset: u32 },
}

/// Partition sizes from the Windows known-device table, used when GetVersion
/// does not report one. MERL: 1:0x5000, 2:0x21000, 3 and 4:0x7f000.
fn known_partition_size(vendor_id: u16, product_id: u16, component: u32) -> Option<u32> {
    match (vendor_id, product_id, component) {
        (0x045e, 0x0b12, 1) => Some(0x5000),
        (0x045e, 0x0b12, 2) => Some(0x21000),
        (0x045e, 0x0b12, 3 | 4) => Some(0x7f000),
        _ => None,
    }
}

/// Component order chosen by the Windows updater once it knows the device's
/// versions of components 2 and 3. Unknown layouts keep manifest order.
pub fn update_order<'a>(
    package: &'a Package,
    device: &BTreeMap<u32, ComponentVersion>,
) -> Vec<&'a Component> {
    let candidates: Vec<&Component> =
        package.components.iter().filter(|c| !c.is_critical()).collect();
    let partition3 = device
        .get(&3)
        .and_then(|v| v.partition_size)
        .or_else(|| known_partition_size(package.vendor_id, package.product_id, 3));
    let manifest3 = candidates.iter().find(|c| c.id == 3).map(|c| c.data.len() as u32);

    let ids: &[u32] = match (partition3, manifest3) {
        (Some(partition), Some(size)) => {
            if !device.contains_key(&3) || (device.contains_key(&2) && size < partition) {
                &[4, 7, 3, 2, 1]
            } else {
                &[2, 1, 4, 7, 3]
            }
        }
        _ => &[],
    };
    let mut ordered: Vec<&Component> = ids
        .iter()
        .filter_map(|id| candidates.iter().copied().find(|c| c.id == *id))
        .collect();
    for component in candidates {
        if !ordered.iter().any(|c| c.id == component.id) {
            ordered.push(component);
        }
    }
    ordered
}

pub struct Updater<'a, L: Link> {
    link: &'a mut L,
    options: Options,
    sequence: Sequence,
    pending: VecDeque<Packet>,
    events: &'a mut dyn FnMut(&Event),
}

impl<'a, L: Link> Updater<'a, L> {
    pub fn new(link: &'a mut L, options: Options, events: &'a mut dyn FnMut(&Event)) -> Self {
        Self { link, options, sequence: Sequence::default(), pending: VecDeque::new(), events }
    }

    fn emit(&mut self, event: Event) {
        (self.events)(&event);
    }

    /// Next device packet before `deadline`. ACKs packets that request one.
    fn recv_packet(&mut self, deadline: Instant) -> Result<Option<Packet>, String> {
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let Some(bytes) = self.link.recv(remaining)? else {
                return Ok(None);
            };
            let Some(packet) = Packet::parse(&bytes) else {
                continue;
            };
            if packet.wants_ack() {
                let received = packet.payload.len() as u32;
                self.link.send(&frame::ack(&packet, received, 0))?;
            }
            return Ok(Some(packet));
        }
    }

    fn wait_ack(&mut self, deadline: Instant) -> Result<Option<Ack>, String> {
        while let Some(packet) = self.recv_packet(deadline)? {
            match Ack::parse(&packet) {
                Some(ack) if ack.command == frame::CMD_FIRMWARE => return Ok(Some(ack)),
                Some(_) => {}
                None => self.pending.push_back(packet),
            }
        }
        Ok(None)
    }

    fn send_set_device_state(&mut self, state: u8) -> Result<(), String> {
        let sequence = self.sequence.take();
        self.link.send(&frame::packet(
            frame::CMD_SET_DEVICE_STATE,
            frame::FLAG_SYSTEM,
            sequence,
            &[state],
        ))
    }

    /// Send a UFU request: one packet, or fragments with ACK handshakes.
    fn send_message(&mut self, payload: &[u8]) -> Result<(), String> {
        let flags = frame::FLAG_SYSTEM | frame::FLAG_ACME;
        let sequence = self.sequence.take();
        if payload.len() <= frame::PACKET_MAX - 4 {
            return self.link.send(&frame::packet(frame::CMD_FIRMWARE, flags, sequence, payload));
        }

        let total = payload.len();
        let (mut offset, mut acked, mut since_ack, mut failures) = (0usize, 0usize, 0u32, 0u32);
        while offset < total {
            let length = frame::FRAGMENT_DATA.min(total - offset);
            let end = offset + length;
            // ACK the first and last fragment, and every fifth in between.
            let want_ack = offset == 0 || end == total || since_ack >= 4;
            self.link.send(&frame::fragment(
                frame::CMD_FIRMWARE,
                frame::FLAG_SYSTEM,
                sequence,
                total as u32,
                offset as u32,
                &payload[offset..end],
                want_ack,
            ))?;
            offset = end;
            since_ack += 1;
            if !want_ack {
                continue;
            }
            since_ack = 0;
            let deadline = Instant::now() + self.options.ack_timeout;
            match self.wait_ack(deadline)? {
                Some(ack) if ack.received as usize >= offset => acked = offset,
                Some(ack) => {
                    // The device reports contiguous data; resend from there.
                    failures += 1;
                    acked = (ack.received as usize).min(offset);
                    offset = acked;
                    self.emit(Event::FragmentResend { offset: offset as u32 });
                }
                None => {
                    failures += 1;
                    offset = acked;
                    self.emit(Event::FragmentResend { offset: offset as u32 });
                }
            }
            if failures > 8 {
                return Err(format!("fragment transfer stalled at byte {acked} of {total}"));
            }
        }
        self.link.send(&frame::end_of_fragments(
            frame::CMD_FIRMWARE,
            frame::FLAG_SYSTEM,
            sequence,
            total as u32,
        ))
    }

    /// Send a request and wait for the matching response, with retries.
    fn request(&mut self, payload: &[u8]) -> Result<(Status, Vec<u8>), String> {
        let op = payload[0];
        let min_len = ufu::MIN_RESPONSE.get(op as usize).copied().unwrap_or(2);
        let timeout = if op == ufu::WRITE_BLOCK {
            self.options.write_timeout
        } else {
            self.options.response_timeout
        };
        for attempt in 1..=self.options.attempts {
            if attempt > 1 {
                self.emit(Event::Retry { op, attempt });
            }
            self.send_message(payload)?;
            let deadline = Instant::now() + timeout;
            loop {
                let packet = match self.pending.pop_front() {
                    Some(packet) => Some(packet),
                    None => self.recv_packet(deadline)?,
                };
                let Some(packet) = packet else { break };
                let response = &packet.payload;
                if packet.command == frame::CMD_FIRMWARE
                    && !packet.is_fragment()
                    && response.len() >= min_len
                    && response[0] == op
                {
                    let status = Status::from_u8(response[1]).ok_or_else(|| {
                        format!("{} returned unknown status {}", ufu::op_name(op), response[1])
                    })?;
                    return Ok((status, response.clone()));
                }
            }
        }
        Err(format!(
            "no response to {} after {} attempts",
            ufu::op_name(op),
            self.options.attempts
        ))
    }

    fn expect_ok(&mut self, payload: &[u8]) -> Result<Vec<u8>, String> {
        let (status, response) = self.request(payload)?;
        if status != Status::Ok {
            return Err(format!("{} failed with status {status:?}", ufu::op_name(payload[0])));
        }
        Ok(response)
    }

    fn get_version(&mut self, component: u32) -> Result<ComponentVersion, String> {
        let response = self.expect_ok(&ufu::get_version(component))?;
        let version = ComponentVersion::parse(&response)
            .ok_or_else(|| "malformed GetVersion response".to_owned())?;
        self.emit(Event::DeviceVersion { component, version: version.version });
        Ok(version)
    }

    /// Wait for a power-off status (bits 6-7 clear), as after a reset.
    fn wait_power_off(&mut self, deadline: Instant) -> Result<bool, String> {
        while let Some(packet) = self.recv_packet(deadline)? {
            if packet.command == frame::CMD_STATUS
                && packet.payload.first().is_some_and(|status| status & 0xc0 == 0)
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// GIP-level reset in the middle of an update: power-off status, then a
    /// Hello, then Start.
    fn reset_and_resume(&mut self) -> Result<(), String> {
        self.emit(Event::Reset);
        self.send_set_device_state(frame::STATE_RESET)?;
        let deadline = Instant::now() + self.options.reset_timeout;
        if !self.wait_power_off(deadline)? {
            return Err("device did not power off after reset".to_owned());
        }
        loop {
            match self.recv_packet(deadline)? {
                Some(packet) if packet.command == frame::CMD_HELLO => break,
                Some(_) => {}
                None => return Err("device did not send Hello after reset".to_owned()),
            }
        }
        self.pending.clear();
        self.send_set_device_state(frame::STATE_START)
    }

    /// Write one component. Returns the number of blocks written.
    fn update_component(&mut self, component: &Component, block: u32) -> Result<u32, String> {
        let size = component.data.len() as u32;
        let begin =
            ufu::begin_update(component.id, component.target_address, size, component.version);
        let mut resets = 0;
        loop {
            let (status, _) = self.request(&begin)?;
            match status {
                Status::Ok => break,
                Status::ComponentInUse => {
                    self.emit(Event::Skipped { component: component.id, reason: "in use" });
                    return Ok(0);
                }
                Status::MustReset if resets < 3 => {
                    resets += 1;
                    self.reset_and_resume()?;
                }
                other => return Err(format!("BeginUpdate failed with status {other:?}")),
            }
        }
        self.emit(Event::Begin { component: component.id, version: component.version, size });

        let mut written = 0;
        let mut offset = 0u32;
        while offset < size {
            let length = block.min(size - offset);
            let address = component.target_address + offset;
            let data = &component.data[offset as usize..(offset + length) as usize];
            let hash = self.expect_ok(&ufu::get_block_hash(address, length))?;
            let needs_write = hash[2..22] != Sha1::digest(data)[..];
            if needs_write {
                self.expect_ok(&ufu::write_block(address, data))?;
                written += 1;
            }
            offset += length;
            self.emit(Event::Block {
                component: component.id,
                address,
                done: offset,
                total: size,
                written: needs_write,
            });
        }

        let (status, _) = self.request(&ufu::complete_update())?;
        match status {
            Status::Ok => {}
            Status::MustReset => self.reset_and_resume()?,
            other => return Err(format!("CompleteUpdate failed with status {other:?}")),
        }
        self.emit(Event::Complete { component: component.id });
        Ok(written)
    }

    /// Update the device to `package`. Ends with the final reset; the device
    /// then reboots into the new firmware.
    pub fn run(&mut self, package: &Package) -> Result<(), String> {
        let response = self.expect_ok(&ufu::get_info())?;
        let info = Info::parse(&response).ok_or_else(|| "malformed GetInfo response".to_owned())?;
        self.emit(Event::Info(info));
        if info.protocol == 1 {
            return Err("device uses the legacy UFU protocol 1, which is not supported".to_owned());
        }
        if (info.vendor_id, info.product_id) != (package.vendor_id, package.product_id) {
            return Err(format!(
                "package is for {:04x}:{:04x} but the device reports {:04x}:{:04x}",
                package.vendor_id, package.product_id, info.vendor_id, info.product_id
            ));
        }
        let block = info.block_size();
        if block == 0 {
            return Err("device reported a zero write size".to_owned());
        }

        let mut device = BTreeMap::new();
        for component in package.components.iter().filter(|c| !c.is_critical()) {
            device.insert(component.id, self.get_version(component.id)?);
        }
        let order = update_order(package, &device);
        self.emit(Event::Order(order.iter().map(|c| c.id).collect()));

        let mut verified = false;
        for _ in 0..self.options.iterations {
            verified = true;
            for component in &order {
                let version = self.get_version(component.id)?;
                if version.version == component.version {
                    self.emit(Event::UpToDate { component: component.id });
                    continue;
                }
                // A component also counts as verified if no block needed writing.
                if self.update_component(component, block)? > 0 {
                    verified = false;
                }
            }
            if verified {
                break;
            }
        }
        if !verified {
            return Err(format!(
                "device still differs from the package after {} passes",
                self.options.iterations
            ));
        }

        // Final reset: the device powers off and reboots into the new
        // firmware. It may drop off USB before the status arrives.
        self.emit(Event::Reset);
        self.send_set_device_state(frame::STATE_RESET)?;
        let deadline = Instant::now() + self.options.reset_timeout;
        match self.wait_power_off(deadline) {
            Ok(true) | Err(_) => Ok(()),
            Ok(false) => Err("device did not power off after the final reset".to_owned()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::{UfuDevice, UfuEndpoint};
    use crate::rdhm::Component;

    /// In-memory controller: UFU endpoint plus GIP reset handling.
    struct MockLink {
        endpoint: UfuEndpoint,
        queue: VecDeque<Vec<u8>>,
        drop_every: Option<usize>,
        fragments_seen: usize,
    }

    impl MockLink {
        fn new(version: Version) -> Self {
            Self {
                endpoint: UfuEndpoint::new(UfuDevice::merlin(version)),
                queue: VecDeque::new(),
                drop_every: None,
                fragments_seen: 0,
            }
        }
    }

    impl Link for MockLink {
        fn send(&mut self, bytes: &[u8]) -> Result<(), String> {
            let packet = Packet::parse(bytes).ok_or("unparsable packet")?;
            match packet.command {
                frame::CMD_FIRMWARE => {
                    if packet.is_fragment() && !packet.is_first_fragment() {
                        self.fragments_seen += 1;
                        if self.drop_every.is_some_and(|n| self.fragments_seen % n == 0) {
                            return Ok(());
                        }
                    }
                    let out = self.endpoint.handle_packet(&packet);
                    self.queue.extend(out);
                }
                frame::CMD_SET_DEVICE_STATE if packet.payload == [frame::STATE_RESET] => {
                    self.queue.push_back(vec![0x03, 0x20, 0x01, 0x04, 0, 0, 0, 0]);
                }
                _ => {}
            }
            Ok(())
        }

        fn recv(&mut self, _timeout: Duration) -> Result<Option<Vec<u8>>, String> {
            Ok(self.queue.pop_front())
        }
    }

    fn package() -> Package {
        let data = |seed: u8, len: usize| (0..len).map(|i| (i as u8).wrapping_mul(seed)).collect();
        let version = Version([5, 23, 6, 0]);
        Package {
            vendor_id: 0x045e,
            product_id: 0x0b12,
            version,
            components: vec![
                Component { id: 2, flags: 0, target_address: 0x1000_5000, version, data: data(3, 0x2100) },
                Component { id: 4, flags: 0, target_address: 0x1002_7000, version: Version([5, 23, 330, 0]), data: data(5, 0x3000) },
                Component { id: 3, flags: 0, target_address: 0x1002_7000, version, data: data(7, 0x3004) },
            ],
        }
    }

    fn run(link: &mut MockLink, package: &Package) -> (Result<(), String>, Vec<Event>) {
        let mut events = Vec::new();
        let mut record = |event: &Event| events.push(event.clone());
        let options = Options { ack_timeout: Duration::ZERO, ..Options::default() };
        let result = Updater::new(link, options, &mut record).run(package);
        (result, events)
    }

    #[test]
    fn updates_in_windows_order_and_matches_image() {
        let package = package();
        let mut link = MockLink::new(Version([5, 17, 3202, 0]));
        let (result, events) = run(&mut link, &package);
        result.unwrap();
        assert!(events.contains(&Event::Order(vec![4, 3, 2])));
        let device = &link.endpoint.device;
        for component in &package.components {
            assert_eq!(device.version(component.id), Some(component.version));
        }
        // Components 4 and 3 share an address; 3 is written last.
        for id in [3, 2] {
            let c = package.components.iter().find(|c| c.id == id).unwrap();
            assert_eq!(device.flash(c.target_address, c.data.len() as u32).unwrap(), &c.data[..]);
        }
    }

    #[test]
    fn up_to_date_device_writes_nothing() {
        let package = package();
        let mut link = MockLink::new(Version([5, 17, 3202, 0]));
        run(&mut link, &package).0.unwrap();
        let (result, events) = run(&mut link, &package);
        result.unwrap();
        assert!(!events.iter().any(|e| matches!(e, Event::Begin { .. })));
    }

    /// `GIP_TEST_PACKAGE=<rdhm file> cargo test -p gip -- --ignored`
    #[test]
    #[ignore = "needs an official rdhm package"]
    fn updates_with_official_package() {
        let path = std::env::var("GIP_TEST_PACKAGE").expect("set GIP_TEST_PACKAGE");
        let package = Package::parse(&std::fs::read(path).unwrap()).unwrap();
        let mut link = MockLink::new(Version([5, 17, 3202, 0]));
        let (result, events) = run(&mut link, &package);
        result.unwrap();
        let writes = events.iter().filter(|e| matches!(e, Event::Block { written: true, .. })).count();
        eprintln!("order {:?}, {writes} blocks written", events.iter().find(|e| matches!(e, Event::Order(_))));
        let device = &link.endpoint.device;
        for component in package.components.iter().filter(|c| !c.is_critical()) {
            assert_eq!(device.version(component.id), Some(component.version));
        }
    }

    #[test]
    fn resends_lost_fragments() {
        let package = package();
        let mut link = MockLink::new(Version([5, 17, 3202, 0]));
        link.drop_every = Some(23);
        let (result, events) = run(&mut link, &package);
        result.unwrap();
        assert!(events.iter().any(|e| matches!(e, Event::FragmentResend { .. })));
        let c = &package.components[0];
        assert_eq!(
            link.endpoint.device.flash(c.target_address, c.data.len() as u32).unwrap(),
            &c.data[..]
        );
    }
}
