//! Host side of UFU: the update sequence the Windows updater runs, as
//! documented in `docs/firmware-update-host.md`.

use crate::gip::frame::{self, Ack, Packet, Sequence};
use crate::gip::rdhm::{Component, Package, Version};
use crate::gip::ufu::{self, ComponentVersion, Info, Status};
use sha1::{Digest, Sha1};
use std::collections::{BTreeMap, VecDeque};
use std::time::{Duration, Instant};

/// A packet transport to one controller's GIP interrupt endpoints.
pub trait Link {
    fn send(&mut self, packet: &[u8]) -> Result<(), String>;
    /// Next device packet, or `None` after `timeout`. An error usually means
    /// the device disconnected.
    fn recv(&mut self, timeout: Duration) -> Result<Option<Vec<u8>>, String>;
    /// Reopen the device after it restarted and re-enumerated on USB, as the
    /// physical controller does after a reset.
    fn reconnect(&mut self, _timeout: Duration) -> Result<(), String> {
        Err("this link cannot reconnect".to_owned())
    }
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
            reset_timeout: Duration::from_secs(120),
            iterations: 10,
        }
    }
}

/// Progress reported while updating.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Info(Info),
    /// Every component matches the package after updating.
    Verified,
    DeviceVersion {
        component: u32,
        version: Version,
    },
    Order(Vec<u32>),
    UpToDate {
        component: u32,
    },
    Begin {
        component: u32,
        version: Version,
        size: u32,
    },
    Block {
        component: u32,
        address: u32,
        done: u32,
        total: u32,
        written: bool,
    },
    Complete {
        component: u32,
    },
    Skipped {
        component: u32,
        reason: &'static str,
    },
    Reset,
    Reconnected,
    Retry {
        op: u8,
        attempt: u32,
    },
    FragmentResend {
        offset: u32,
    },
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
    let candidates: Vec<&Component> = package
        .components
        .iter()
        .filter(|c| !c.is_critical())
        .collect();
    let partition3 = device
        .get(&3)
        .and_then(|v| v.partition_size)
        .or_else(|| known_partition_size(package.vendor_id, package.product_id, 3));
    let manifest3 = candidates
        .iter()
        .find(|c| c.id == 3)
        .map(|c| c.data.len() as u32);

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

/// What an update would change, in update order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub info: Info,
    pub components: Vec<PlannedComponent>,
    /// Why components larger than their reported partition were accepted.
    pub notes: Vec<String>,
}

/// Accept a component larger than the partition the device reports only
/// when nothing that matters is in the way:
///
/// - the device reports the partition at the package's target address,
/// - everything beyond the reported size is erased padding (0xff), and
/// - that excess overlaps no other component's reported partition.
///
/// This is MERL going from 5.9 to 5.23. Component 2's own image header
/// (`sdhi` at +0x100) holds the size the device reports: 0x21000 on 5.9,
/// 0x22000 on 5.23. 5.23 moves the application from 0x10026000 to
/// 0x10027000 (written first, as components 4 and 3), and component 2 then
/// grows into the freed page; its last 0x119a+0x1000 bytes are 0xff. Returns
/// a note describing the growth, or why the component does not fit.
fn check_fits(
    component: &Component,
    layout: &BTreeMap<u32, ComponentVersion>,
) -> Result<String, String> {
    let size = component.data.len() as u64;
    let reported = layout
        .get(&component.id)
        .and_then(|v| v.partition_size)
        .ok_or_else(|| format!("component {} no longer reports its partition", component.id))?;
    let refuse = |why: String| {
        Err(format!(
            "component {} is {size:#x} bytes but the device's partition holds {reported:#x}; \
             {why}; this package does not fit this controller",
            component.id
        ))
    };
    match layout[&component.id].partition_address {
        Some(address) if address == component.target_address => {}
        Some(address) => {
            return refuse(format!(
                "the device has it at {address:#x}, the package at {:#x}",
                component.target_address
            ));
        }
        None => return refuse("the device does not report where it starts".to_owned()),
    }
    let excess = &component.data[reported as usize..];
    if excess.iter().any(|&byte| byte != 0xff) {
        return refuse(format!(
            "the {:#x} bytes beyond it are not erased padding",
            excess.len()
        ));
    }
    let start = u64::from(component.target_address) + u64::from(reported);
    let end = u64::from(component.target_address) + size;
    for (id, other) in layout.iter().filter(|(id, _)| **id != component.id) {
        let (Some(address), Some(length)) = (other.partition_address, other.partition_size) else {
            return refuse(format!(
                "component {id} does not report its partition, so an overlap cannot be ruled out"
            ));
        };
        let (address, length) = (u64::from(address), u64::from(length));
        if address < end && start < address + length {
            return refuse(format!(
                "the excess {start:#x}..{end:#x} overlaps component {id}'s partition \
                 {address:#x}..{:#x}",
                address + length
            ));
        }
    }
    Ok(format!(
        "component {} grows from {reported:#x} to {size:#x} bytes: the extra {:#x} bytes \
         ({start:#x}..{end:#x}) are erased padding in flash no other component's partition uses",
        component.id,
        size - u64::from(reported)
    ))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlannedComponent {
    pub id: u32,
    /// Version on the device now.
    pub device: Version,
    /// Version in the package.
    pub package: Version,
}

impl PlannedComponent {
    pub fn needs_update(&self) -> bool {
        self.device != self.package
    }
}

impl Plan {
    pub fn needs_update(&self) -> bool {
        self.components.iter().any(PlannedComponent::needs_update)
    }
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
        Self {
            link,
            options,
            sequence: Sequence::default(),
            pending: VecDeque::new(),
            events,
        }
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
            return self.link.send(&frame::packet(
                frame::CMD_FIRMWARE,
                flags,
                sequence,
                payload,
            ));
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
                    self.emit(Event::FragmentResend {
                        offset: offset as u32,
                    });
                }
                None => {
                    failures += 1;
                    offset = acked;
                    self.emit(Event::FragmentResend {
                        offset: offset as u32,
                    });
                }
            }
            if failures > 8 {
                return Err(format!(
                    "fragment transfer stalled at byte {acked} of {total}"
                ));
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
                        format!(
                            "{} returned unknown status {}",
                            ufu::op_name(op),
                            response[1]
                        )
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
            return Err(format!(
                "{} failed with status {status:?}",
                ufu::op_name(payload[0])
            ));
        }
        Ok(response)
    }

    /// GetInfo, without starting an update.
    pub fn query_info(&mut self) -> Result<Info, String> {
        let response = self.expect_ok(&ufu::get_info())?;
        Info::parse(&response).ok_or_else(|| "malformed GetInfo response".to_owned())
    }

    /// GetVersion for one component; `None` if the device does not have it.
    pub fn query_version(&mut self, component: u32) -> Result<Option<ComponentVersion>, String> {
        match self.request(&ufu::get_version(component))? {
            (Status::Ok, response) => ComponentVersion::parse(&response)
                .map(Some)
                .ok_or_else(|| "malformed GetVersion response".to_owned()),
            (Status::UnknownComponent, _) => Ok(None),
            (status, _) => Err(format!("GetVersion failed with status {status:?}")),
        }
    }

    fn get_version(&mut self, component: u32) -> Result<ComponentVersion, String> {
        let response = self.expect_ok(&ufu::get_version(component))?;
        let version = ComponentVersion::parse(&response)
            .ok_or_else(|| "malformed GetVersion response".to_owned())?;
        self.emit(Event::DeviceVersion {
            component,
            version: version.version,
        });
        Ok(version)
    }

    /// Wait for a power-off status (bits 6-7 clear), as after a reset.
    fn wait_power_off(&mut self, deadline: Instant) -> Result<bool, String> {
        while let Some(packet) = self.recv_packet(deadline)? {
            if packet.command == frame::CMD_STATUS
                && packet
                    .payload
                    .first()
                    .is_some_and(|status| status & 0xc0 == 0)
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// How long a restarting device gets to come back. Windows waits twice as
    /// long after component 7 (the Adaptive Controller's extra image).
    fn reset_timeout(&self, component: u32) -> Duration {
        if component == 7 {
            self.options.reset_timeout * 2
        } else {
            self.options.reset_timeout
        }
    }

    /// Reset in the middle of an update (after `MustReset`), then Start.
    ///
    /// The device sends a power-off status, then either a Hello on the same
    /// connection (the Windows driver's model) or, like the physical
    /// controller, drops off USB and re-enumerates. In that case the link is
    /// reopened quickly: a restarted controller that nobody starts goes back
    /// to sleep after a while.
    fn reset_and_resume(&mut self, component: u32) -> Result<(), String> {
        self.emit(Event::Reset);
        self.send_set_device_state(frame::STATE_RESET)?;
        let deadline = Instant::now() + self.reset_timeout(component);
        let mut powered_off = false;
        loop {
            match self.recv_packet(deadline) {
                Ok(Some(packet)) if packet.command == frame::CMD_HELLO && powered_off => break,
                Ok(Some(packet)) => {
                    if packet.command == frame::CMD_STATUS
                        && packet
                            .payload
                            .first()
                            .is_some_and(|status| status & 0xc0 == 0)
                    {
                        powered_off = true;
                    }
                }
                Ok(None) => {
                    return Err(if powered_off {
                        "device did not come back after reset".to_owned()
                    } else {
                        "device did not power off after reset".to_owned()
                    });
                }
                Err(_) => {
                    self.link
                        .reconnect(deadline.saturating_duration_since(Instant::now()))?;
                    self.emit(Event::Reconnected);
                    break;
                }
            }
        }
        self.pending.clear();
        self.send_set_device_state(frame::STATE_START)
    }

    /// Write one component. Returns the number of blocks written.
    fn update_component(&mut self, component: &Component, block: u32) -> Result<u32, String> {
        let size = component.data.len() as u32;
        let begin = ufu::begin_update(
            component.id,
            component.target_address,
            size,
            component.version,
        );
        let mut resets = 0;
        loop {
            let (status, _) = self.request(&begin)?;
            match status {
                Status::Ok => break,
                Status::ComponentInUse => {
                    self.emit(Event::Skipped {
                        component: component.id,
                        reason: "in use",
                    });
                    return Ok(0);
                }
                Status::MustReset if resets < 3 => {
                    resets += 1;
                    self.reset_and_resume(component.id)?;
                }
                other => return Err(format!("BeginUpdate failed with status {other:?}")),
            }
        }
        self.emit(Event::Begin {
            component: component.id,
            version: component.version,
            size,
        });

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
            Status::MustReset => self.reset_and_resume(component.id)?,
            other => return Err(format!("CompleteUpdate failed with status {other:?}")),
        }
        self.emit(Event::Complete {
            component: component.id,
        });
        Ok(written)
    }

    /// Read-only check of the device against `package`: GetInfo, the
    /// device's component versions, and the update order. Fails if the
    /// package does not belong to or fit this device.
    pub fn plan(&mut self, package: &Package) -> Result<Plan, String> {
        let info = self.query_info()?;
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
        if info.block_size() == 0 {
            return Err("device reported a zero write size".to_owned());
        }

        let mut device = BTreeMap::new();
        for component in package.components.iter().filter(|c| !c.is_critical()) {
            device.insert(component.id, self.get_version(component.id)?);
        }
        // Refuse packages built for another flash layout before writing
        // anything (e.g. MERL 5.3's 0x7f000 images on 0x7e000 partitions),
        // unless the excess is padding into unused flash; see `check_fits`.
        let mut notes = Vec::new();
        let mut layout: Option<BTreeMap<u32, ComponentVersion>> = None;
        for component in package.components.iter().filter(|c| !c.is_critical()) {
            let Some(reported) = device[&component.id].partition_size else {
                continue;
            };
            if component.data.len() as u64 <= u64::from(reported) {
                continue;
            }
            // Only now ask about every component, including ones the package
            // leaves out: the excess must not touch any of them.
            if layout.is_none() {
                let mut all = BTreeMap::new();
                for id in 1..=7 {
                    if let Some(version) = self.query_version(id)? {
                        all.insert(id, version);
                    }
                }
                layout = Some(all);
            }
            notes.push(check_fits(component, layout.as_ref().unwrap())?);
        }
        let order = update_order(package, &device);
        self.emit(Event::Order(order.iter().map(|c| c.id).collect()));
        Ok(Plan {
            info,
            notes,
            components: order
                .iter()
                .map(|component| PlannedComponent {
                    id: component.id,
                    device: device[&component.id].version,
                    package: component.version,
                })
                .collect(),
        })
    }

    /// Update the device to `package`. Returns `false` if every component was
    /// already up to date; nothing is written and the device is not reset.
    /// Otherwise ends with the final reset, after which the device reboots
    /// into the new firmware.
    pub fn run(&mut self, package: &Package) -> Result<bool, String> {
        let plan = self.plan(package)?;
        let block = plan.info.block_size();
        let order: Vec<&Component> = plan
            .components
            .iter()
            .filter_map(|planned| package.components.iter().find(|c| c.id == planned.id))
            .collect();

        let mut verified = false;
        let mut changed = false;
        for _ in 0..self.options.iterations {
            verified = true;
            for component in &order {
                let version = self.get_version(component.id)?;
                if version.version == component.version {
                    self.emit(Event::UpToDate {
                        component: component.id,
                    });
                    continue;
                }
                // A component also counts as verified if no block needed writing.
                changed = true;
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

        if !changed {
            return Ok(false);
        }
        self.emit(Event::Verified);

        // Final reset: the device powers off and reboots into the new
        // firmware. It may drop off USB before the status arrives.
        self.emit(Event::Reset);
        self.send_set_device_state(frame::STATE_RESET)?;
        let deadline = Instant::now() + self.options.reset_timeout;
        match self.wait_power_off(deadline) {
            Ok(true) | Err(_) => Ok(true),
            Ok(false) => Err("device did not power off after the final reset".to_owned()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gip::device::{UfuDevice, UfuEndpoint};
    use crate::gip::rdhm::Component;

    /// In-memory controller: UFU endpoint plus GIP reset handling. Like the
    /// physical controller, it drops off USB after a reset.
    struct MockLink {
        endpoint: UfuEndpoint,
        queue: VecDeque<Vec<u8>>,
        drop_every: Option<usize>,
        fragments_seen: usize,
        disconnected: bool,
        can_reconnect: bool,
        reconnects: usize,
        /// After a restart: the single Hello was sent and must be answered
        /// with Start before anything else.
        awaiting_start: bool,
        /// The Hello went unanswered; the controller ignores the host.
        stuck: bool,
    }

    impl MockLink {
        fn new(version: Version) -> Self {
            Self {
                endpoint: UfuEndpoint::new(UfuDevice::merlin(version)),
                queue: VecDeque::new(),
                drop_every: None,
                fragments_seen: 0,
                disconnected: false,
                can_reconnect: true,
                reconnects: 0,
                awaiting_start: false,
                stuck: false,
            }
        }

        /// Unplug and plug back in: a fresh, responsive controller.
        fn replug(&mut self) {
            self.disconnected = false;
            self.queue.clear();
            self.awaiting_start = false;
            self.stuck = false;
        }
    }

    impl Link for MockLink {
        fn send(&mut self, bytes: &[u8]) -> Result<(), String> {
            if self.disconnected {
                return Err("device disconnected".to_owned());
            }
            let packet = Packet::parse(bytes).ok_or("unparsable packet")?;
            if self.awaiting_start {
                self.awaiting_start = false;
                if (packet.command, packet.payload.as_slice())
                    != (frame::CMD_SET_DEVICE_STATE, &[frame::STATE_START][..])
                {
                    self.stuck = true;
                }
            }
            if self.stuck {
                return Ok(());
            }
            match packet.command {
                frame::CMD_FIRMWARE => {
                    if packet.is_fragment() && !packet.is_first_fragment() {
                        self.fragments_seen += 1;
                        if self
                            .drop_every
                            .is_some_and(|n| self.fragments_seen.is_multiple_of(n))
                        {
                            return Ok(());
                        }
                    }
                    let out = self.endpoint.handle_packet(&packet);
                    self.queue.extend(out);
                }
                frame::CMD_SET_DEVICE_STATE if packet.payload == [frame::STATE_RESET] => {
                    self.queue
                        .push_back(vec![0x03, 0x20, 0x01, 0x04, 0, 0, 0, 0]);
                    self.disconnected = true;
                }
                _ => {}
            }
            Ok(())
        }

        fn recv(&mut self, _timeout: Duration) -> Result<Option<Vec<u8>>, String> {
            match self.queue.pop_front() {
                Some(packet) => Ok(Some(packet)),
                None if self.disconnected => Err("device disconnected".to_owned()),
                None => Ok(None),
            }
        }

        fn reconnect(&mut self, _timeout: Duration) -> Result<(), String> {
            if !self.can_reconnect {
                return Err("cannot reconnect".to_owned());
            }
            self.disconnected = false;
            self.queue.clear();
            self.reconnects += 1;
            // A restarted controller sends one Hello and waits for Start.
            self.queue.push_back(vec![
                0x02, 0x20, 0x01, 0x1c, 0x7e, 0xed, 0x8d, 0xa3, 0x8d, 0xaa, 0x00, 0x00, 0x5e, 0x04,
                0x12, 0x0b, 0x05, 0x00, 0x17, 0x00, 0x06, 0x00, 0x00, 0x00, 0x08, 0x04, 0x01, 0x00,
                0x01, 0x00, 0x01, 0x00,
            ]);
            self.awaiting_start = true;
            Ok(())
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
                Component {
                    id: 2,
                    flags: 0,
                    target_address: 0x1000_5000,
                    version,
                    data: data(3, 0x2100),
                },
                Component {
                    id: 4,
                    flags: 0,
                    target_address: 0x1002_7000,
                    version: Version([5, 23, 330, 0]),
                    data: data(5, 0x3000),
                },
                Component {
                    id: 3,
                    flags: 0,
                    target_address: 0x1002_7000,
                    version,
                    data: data(7, 0x3004),
                },
            ],
        }
    }

    fn run(link: &mut MockLink, package: &Package) -> (Result<bool, String>, Vec<Event>) {
        let mut events = Vec::new();
        let mut record = |event: &Event| events.push(event.clone());
        let options = Options {
            ack_timeout: Duration::ZERO,
            ..Options::default()
        };
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
            assert_eq!(
                device.flash(c.target_address, c.data.len() as u32).unwrap(),
                &c.data[..]
            );
        }
    }

    #[test]
    fn resumes_after_must_reset_and_usb_restart() {
        // Component 2 answers CompleteUpdate with MustReset, as the physical
        // controller does; the update must reconnect and finish 4 and 3.
        let package = package();
        let mut link = MockLink::new(Version([5, 17, 3202, 0]));
        let (result, events) = run(&mut link, &package);
        assert_eq!(result, Ok(true));
        // The physical controller's sequence: restarts after CompleteUpdate 2,
        // BeginUpdate 4, CompleteUpdate 4 and CompleteUpdate 3. Each restart's
        // Hello must be answered with Start, or the mock gets stuck.
        assert!(events.contains(&Event::Reconnected));
        assert_eq!(link.reconnects, 4);
        assert!(!link.stuck);
        for component in &package.components {
            assert_eq!(
                link.endpoint.device.version(component.id),
                Some(component.version)
            );
        }
    }

    #[test]
    fn must_reset_without_reconnect_fails() {
        let package = package();
        let mut link = MockLink::new(Version([5, 17, 3202, 0]));
        link.can_reconnect = false;
        assert!(run(&mut link, &package).0.is_err());
    }

    #[test]
    fn up_to_date_device_writes_nothing() {
        let package = package();
        let mut link = MockLink::new(Version([5, 17, 3202, 0]));
        run(&mut link, &package).0.unwrap();
        // The final reset restarted the mock controller; plug it back in.
        link.replug();
        let (result, events) = run(&mut link, &package);
        assert_eq!(result, Ok(false));
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, Event::Begin { .. } | Event::Reset))
        );
    }

    /// A MERL controller after 4 and 3 went to 5.23 but 2 is still 5.9: it
    /// reports component 2's partition as 0x21000, the old size. The package
    /// stand-in has 0x2100 bytes of component 2, so model 0x2000 + padding.
    fn old_component2_device() -> MockLink {
        let mut link = MockLink::new(Version([5, 17, 3202, 0]));
        run(&mut link, &package()).0.unwrap();
        link.replug();
        link.endpoint.device.set_partition(2, 0x1000_5000, 0x2000);
        link
    }

    /// Package whose component 2 is `0x2000` bytes of image and `0x100`
    /// bytes of `tail`, and a version the device does not have yet.
    fn package_with_component2_tail(tail: u8) -> Package {
        let mut package = package();
        let c2 = package.components.iter_mut().find(|c| c.id == 2).unwrap();
        c2.data[0x2000..].fill(tail);
        c2.version = Version([5, 23, 7, 0]);
        package
    }

    #[test]
    fn grows_component_into_unused_flash_when_excess_is_padding() {
        let package = package_with_component2_tail(0xff);
        let mut link = old_component2_device();
        let (result, events) = run(&mut link, &package);
        assert_eq!(result, Ok(true));
        assert!(events.contains(&Event::Begin {
            component: 2,
            version: Version([5, 23, 7, 0]),
            size: 0x2100,
        }));
        let c2 = package.components.iter().find(|c| c.id == 2).unwrap();
        assert_eq!(
            link.endpoint.device.flash(c2.target_address, 0x2100).unwrap(),
            &c2.data[..]
        );
    }

    #[test]
    fn refuses_growth_when_excess_is_not_padding() {
        let package = package_with_component2_tail(0x00);
        let mut link = old_component2_device();
        let (result, events) = run(&mut link, &package);
        assert!(result.unwrap_err().contains("not erased padding"));
        assert!(!events.iter().any(|e| matches!(e, Event::Begin { .. })));
    }

    #[test]
    fn refuses_growth_into_another_partition() {
        // As if the 5.9 application still started right after component 2.
        let package = package_with_component2_tail(0xff);
        let mut link = old_component2_device();
        link.endpoint.device.set_partition(3, 0x1000_7000, 0x7f000);
        let (result, events) = run(&mut link, &package);
        assert!(result.unwrap_err().contains("overlaps component 3"));
        assert!(!events.iter().any(|e| matches!(e, Event::Begin { .. })));
    }

    #[test]
    fn refuses_growth_at_another_address() {
        let package = package_with_component2_tail(0xff);
        let mut link = old_component2_device();
        link.endpoint.device.set_partition(2, 0x1000_6000, 0x2000);
        let (result, _) = run(&mut link, &package);
        assert!(result.unwrap_err().contains("the device has it at"));
    }

    #[test]
    fn refuses_component_larger_than_partition() {
        let mut package = package();
        package.components[2].data = vec![0; 0x7f000];
        let mut link = MockLink::new(Version([5, 17, 3202, 0]));
        let (result, events) = run(&mut link, &package);
        assert!(result.unwrap_err().contains("does not fit"));
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
        let writes = events
            .iter()
            .filter(|e| matches!(e, Event::Block { written: true, .. }))
            .count();
        eprintln!(
            "order {:?}, {writes} blocks written",
            events.iter().find(|e| matches!(e, Event::Order(_)))
        );
        let device = &link.endpoint.device;
        for component in package.components.iter().filter(|c| !c.is_critical()) {
            assert_eq!(device.version(component.id), Some(component.version));
        }
    }

    /// The state of a physical controller after `update --latest` stopped on
    /// the partition check: 1 and 2 on 5.9.2709.0 (2 reporting 0x21000 at
    /// 0x10005000), 3 and 4 on 5.23 at 0x10027000. The official package must
    /// now be accepted, write only component 2, and leave 3 untouched.
    /// `GIP_TEST_PACKAGE=<MERL 5.23.6.0 rdhm> cargo test -- --ignored`
    #[test]
    #[ignore = "needs an official rdhm package"]
    fn finishes_merl_5_9_to_5_23_with_official_package() {
        let path = std::env::var("GIP_TEST_PACKAGE").expect("set GIP_TEST_PACKAGE");
        let package = Package::parse(&std::fs::read(path).unwrap()).unwrap();
        let mut link = MockLink::new(Version([5, 17, 3202, 0]));
        run(&mut link, &package).0.unwrap();
        link.replug();
        let old = Version([5, 9, 2709, 0]);
        let device = &mut link.endpoint.device;
        device.set_version(1, old);
        device.set_version(2, old);
        device.set_partition(2, 0x1000_5000, 0x21000);
        let c3 = package.components.iter().find(|c| c.id == 3).unwrap();
        let app_before = device
            .flash(c3.target_address, c3.data.len() as u32)
            .unwrap()
            .to_vec();

        let mut events = Vec::new();
        let mut record = |event: &Event| events.push(event.clone());
        let options = Options {
            ack_timeout: Duration::ZERO,
            ..Options::default()
        };
        let plan = Updater::new(&mut link, options.clone(), &mut record)
            .plan(&package)
            .unwrap();
        assert_eq!(plan.notes.len(), 1, "{:?}", plan.notes);
        assert!(plan.notes[0].contains("0x10026000..0x10027000"), "{}", plan.notes[0]);

        let result = Updater::new(&mut link, options, &mut record).run(&package);
        assert_eq!(result, Ok(true));
        let begins: Vec<u32> = events
            .iter()
            .filter_map(|e| match e {
                Event::Begin { component, .. } => Some(*component),
                _ => None,
            })
            .collect();
        assert_eq!(begins, vec![2]);
        let device = &link.endpoint.device;
        let c2 = package.components.iter().find(|c| c.id == 2).unwrap();
        assert_eq!(device.flash(0x1000_5000, 0x22000).unwrap(), &c2.data[..]);
        assert_eq!(
            device.flash(c3.target_address, c3.data.len() as u32).unwrap(),
            &app_before[..]
        );
        assert_eq!(device.version(2), Some(c2.version));
    }

    #[test]
    fn resends_lost_fragments() {
        let package = package();
        let mut link = MockLink::new(Version([5, 17, 3202, 0]));
        link.drop_every = Some(23);
        let (result, events) = run(&mut link, &package);
        result.unwrap();
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::FragmentResend { .. }))
        );
        let c = &package.components[0];
        assert_eq!(
            link.endpoint
                .device
                .flash(c.target_address, c.data.len() as u32)
                .unwrap(),
            &c.data[..]
        );
    }
}
