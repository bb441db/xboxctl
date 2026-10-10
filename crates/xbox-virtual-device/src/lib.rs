//! Protocol core for a virtual Xbox GIP controller.
//!
//! This module models the captured USB descriptors, the GIP startup exchange
//! and firmware updates. The binary exposes it through Linux FunctionFS.

use gip::{
    device::{Completed, UfuDevice, UfuEndpoint},
    frame::{self, Packet},
};
pub use gip::rdhm::Version;
use std::time::Duration;

/// USB identity of the physical controller used for the current capture set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UsbIdentity {
    pub vendor_id: u16,
    pub product_id: u16,
}

pub const USB_IDENTITY: UsbIdentity = UsbIdentity {
    vendor_id: 0x045e,
    product_id: 0x0b12,
};

/// Parsed representation of the USB device descriptor. Field names follow
/// USB 2.0 so descriptor bytes are auditable instead of being an opaque blob.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceDescriptor {
    pub usb_version_bcd: u16,
    pub device_class: u8,
    pub device_subclass: u8,
    pub device_protocol: u8,
    pub endpoint0_max_packet_size: u8,
    pub vendor_id: u16,
    pub product_id: u16,
    pub device_version_bcd: u16,
    pub manufacturer_string_index: u8,
    pub product_string_index: u8,
    pub serial_string_index: u8,
    pub configuration_count: u8,
}

impl DeviceDescriptor {
    pub const fn xbox_capture() -> Self {
        Self {
            usb_version_bcd: 0x0200,
            device_class: 0xff,
            device_subclass: 0x47,
            device_protocol: 0xd0,
            endpoint0_max_packet_size: 64,
            vendor_id: USB_IDENTITY.vendor_id,
            product_id: USB_IDENTITY.product_id,
            device_version_bcd: 0x0517,
            manufacturer_string_index: 1,
            product_string_index: 2,
            serial_string_index: 3,
            configuration_count: 1,
        }
    }

    pub fn to_bytes(self) -> [u8; 18] {
        let mut out = [0; 18];
        out[0..2].copy_from_slice(&[18, 0x01]);
        out[2..4].copy_from_slice(&self.usb_version_bcd.to_le_bytes());
        out[4..8].copy_from_slice(&[
            self.device_class,
            self.device_subclass,
            self.device_protocol,
            self.endpoint0_max_packet_size,
        ]);
        out[8..10].copy_from_slice(&self.vendor_id.to_le_bytes());
        out[10..12].copy_from_slice(&self.product_id.to_le_bytes());
        out[12..14].copy_from_slice(&self.device_version_bcd.to_le_bytes());
        out[14..18].copy_from_slice(&[
            self.manufacturer_string_index,
            self.product_string_index,
            self.serial_string_index,
            self.configuration_count,
        ]);
        out
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EndpointDescriptor {
    pub address: u8,
    /// USB transfer type bits: 1 is isochronous, 2 bulk, 3 interrupt.
    pub transfer_type: u8,
    pub max_packet_size: u16,
    pub interval: u8,
}

impl EndpointDescriptor {
    fn append_to(self, out: &mut Vec<u8>) {
        out.extend_from_slice(&[7, 0x05, self.address, self.transfer_type]);
        out.extend_from_slice(&self.max_packet_size.to_le_bytes());
        out.push(self.interval);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterfaceDescriptor {
    pub number: u8,
    pub alternate_setting: u8,
    pub class: u8,
    pub subclass: u8,
    pub protocol: u8,
    pub endpoints: Vec<EndpointDescriptor>,
}

impl InterfaceDescriptor {
    fn append_to(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&[
            9,
            0x04,
            self.number,
            self.alternate_setting,
            self.endpoints.len() as u8,
            self.class,
            self.subclass,
            self.protocol,
            0,
        ]);
        for endpoint in &self.endpoints {
            endpoint.append_to(out);
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigurationDescriptor {
    pub configuration_value: u8,
    pub attributes: u8,
    /// USB bMaxPower units are 2 mA.
    pub max_power_units: u8,
    pub interfaces: Vec<InterfaceDescriptor>,
}

impl ConfigurationDescriptor {
    /// Full configuration/interface/endpoint set observed on `045e:0b12`.
    pub fn xbox_capture() -> Self {
        let gip = |number, alternate_setting, interval| InterfaceDescriptor {
            number,
            alternate_setting,
            class: 0xff,
            subclass: 0x47,
            protocol: 0xd0,
            endpoints: vec![
                EndpointDescriptor {
                    address: 0x02,
                    transfer_type: 3,
                    max_packet_size: 64,
                    interval,
                },
                EndpointDescriptor {
                    address: 0x82,
                    transfer_type: 3,
                    max_packet_size: 64,
                    interval,
                },
            ],
        };
        let empty = |number, alternate_setting| InterfaceDescriptor {
            number,
            alternate_setting,
            class: 0xff,
            subclass: 0x47,
            protocol: 0xd0,
            endpoints: Vec::new(),
        };
        Self {
            configuration_value: 1,
            attributes: 0xa0,
            max_power_units: 250,
            interfaces: vec![
                gip(0, 0, 4),
                gip(0, 1, 2),
                empty(1, 0),
                InterfaceDescriptor {
                    number: 1,
                    alternate_setting: 1,
                    class: 0xff,
                    subclass: 0x47,
                    protocol: 0xd0,
                    endpoints: vec![
                        EndpointDescriptor {
                            address: 0x03,
                            transfer_type: 1,
                            max_packet_size: 228,
                            interval: 1,
                        },
                        EndpointDescriptor {
                            address: 0x83,
                            transfer_type: 1,
                            max_packet_size: 64,
                            interval: 1,
                        },
                    ],
                },
                empty(2, 0),
                InterfaceDescriptor {
                    number: 2,
                    alternate_setting: 1,
                    class: 0xff,
                    subclass: 0x47,
                    protocol: 0xd0,
                    endpoints: vec![
                        EndpointDescriptor {
                            address: 0x04,
                            transfer_type: 2,
                            max_packet_size: 64,
                            interval: 0,
                        },
                        EndpointDescriptor {
                            address: 0x84,
                            transfer_type: 2,
                            max_packet_size: 64,
                            interval: 0,
                        },
                    ],
                },
            ],
        }
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let total_length = 9 + self
            .interfaces
            .iter()
            .map(|interface| 9 + interface.endpoints.len() * 7)
            .sum::<usize>();
        let interface_count = self
            .interfaces
            .iter()
            .map(|i| i.number)
            .collect::<std::collections::BTreeSet<_>>()
            .len();
        let mut out = Vec::with_capacity(total_length);
        out.extend_from_slice(&[9, 0x02]);
        out.extend_from_slice(&(total_length as u16).to_le_bytes());
        out.extend_from_slice(&[
            interface_count as u8,
            self.configuration_value,
            0,
            self.attributes,
            self.max_power_units,
        ]);
        for interface in &self.interfaces {
            interface.append_to(&mut out);
        }
        out
    }
}

pub const MANUFACTURER: &str = "Microsoft";
pub const PRODUCT: &str = "Controller";

/// GIP device ID captured from the physical controller, in wire order.
/// Reusing it lets us test whether Windows' post-metadata decision depends on
/// the identity associated with the captured metadata fixture.
pub const CAPTURED_DEVICE_ID: [u8; 8] = [0x7e, 0xed, 0x8d, 0xa3, 0x8d, 0xaa, 0x00, 0x00];

pub const HELLO_INTERVAL: Duration = Duration::from_millis(500);
pub const GIP_ENDPOINT_IN: u8 = 0x82;
pub const GIP_ENDPOINT_OUT: u8 = 0x02;

/// Current state in the GIP startup sequence implemented by this core.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceState {
    /// Waiting for the host; Hello is sent every 500 ms.
    Arrival,
    /// Sending the captured metadata response.
    Metadata,
    /// Metadata was sent; waiting for the host to start the device.
    Idle,
    /// The host sent Set Device State: Start.
    Active,
    /// The host sent Set Device State: Off.
    Off,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MetadataAckPhase {
    AwaitFirstAck,
    AwaitFinalAck,
}

/// Firmware version of the captured controller.
pub const CAPTURED_FIRMWARE: Version = Version([5, 23, 6, 0]);

/// GIP protocol core for the captured `045e:0b12` controller.
///
/// Call [`hello_packet`](Self::hello_packet) at [`HELLO_INTERVAL`] while the
/// device is in Arrival. Pass each host interrupt-OUT message to
/// [`handle_host_packet`](Self::handle_host_packet) and send its returned
/// packets on interrupt-IN. The startup frames are replayed from the captured
/// exchange. Firmware updates (GIP 0x0c, UFU) are handled by a simulated
/// flash, as in the QEMU `usb-xbox-gip` device.
#[derive(Debug)]
pub struct VirtualController {
    state: DeviceState,
    hello_sequence: u8,
    status_sequence: u8,
    metadata_ack_phase: Option<MetadataAckPhase>,
    device_id: [u8; 8],
    firmware: Version,
    ufu: UfuEndpoint,
    completed: Vec<Completed>,
}

impl Default for VirtualController {
    fn default() -> Self {
        Self::new()
    }
}

impl VirtualController {
    pub fn new() -> Self {
        Self::with_device_id(CAPTURED_DEVICE_ID)
    }

    pub fn with_device_id(device_id: [u8; 8]) -> Self {
        Self::with_identity(device_id, CAPTURED_FIRMWARE)
    }

    /// A controller reporting `firmware` in its Hello, metadata and UFU
    /// component versions.
    pub fn with_identity(device_id: [u8; 8], firmware: Version) -> Self {
        Self {
            state: DeviceState::Arrival,
            hello_sequence: 0x02,
            status_sequence: 0x01,
            metadata_ack_phase: None,
            device_id,
            firmware,
            ufu: UfuEndpoint::new(UfuDevice::merlin(firmware)),
            completed: Vec::new(),
        }
    }

    pub const fn firmware(&self) -> Version {
        self.firmware
    }

    /// Firmware components finished since the last call.
    pub fn take_completed(&mut self) -> Vec<Completed> {
        std::mem::take(&mut self.completed)
    }

    /// Simulated flash contents, for checking an update.
    pub fn flash(&self, address: u32, size: u32) -> Option<&[u8]> {
        self.ufu.device.flash(address, size)
    }

    pub const fn state(&self) -> DeviceState {
        self.state
    }

    pub const fn device_id(&self) -> [u8; 8] {
        self.device_id
    }

    /// Return the 32-digit USB serial string containing this virtual ID.
    pub fn serial_number(&self) -> String {
        format!("{:032x}", u64::from_le_bytes(self.device_id))
    }

    pub fn serial_number_descriptor(&self) -> Vec<u8> {
        string_descriptor(&self.serial_number())
    }

    /// Build the device descriptor with this instance's USB identity.
    pub fn device_descriptor(&self) -> [u8; 18] {
        let mut descriptor = DeviceDescriptor::xbox_capture();
        descriptor.device_version_bcd = bcd_device(self.firmware);
        descriptor.to_bytes()
    }

    /// Build the complete captured configuration/interface/endpoint tree.
    pub fn configuration_descriptor(&self) -> Vec<u8> {
        ConfigurationDescriptor::xbox_capture().to_bytes()
    }

    /// Build the 32-byte GIP Hello packet with the current firmware version.
    pub fn hello_packet(&mut self) -> Option<[u8; 32]> {
        if self.state != DeviceState::Arrival {
            return None;
        }

        let seq = self.hello_sequence;
        self.hello_sequence = self.hello_sequence.wrapping_add(1);
        if self.hello_sequence == 0 {
            self.hello_sequence = 1;
        }

        let mut packet = [0u8; 32];
        packet[..4].copy_from_slice(&[0x02, 0x20, seq, 0x1c]);
        packet[4..12].copy_from_slice(&self.device_id);
        packet[12..14].copy_from_slice(&USB_IDENTITY.vendor_id.to_le_bytes());
        packet[14..16].copy_from_slice(&USB_IDENTITY.product_id.to_le_bytes());
        packet[16..24].copy_from_slice(&self.firmware.to_le());
        packet[24..32].copy_from_slice(&[8, 4, 1, 0, 1, 0, 1, 0]);
        Some(packet)
    }

    /// Process one host-to-device GIP packet and return device-to-host packets.
    ///
    /// The metadata request starts a captured multi-packet response. The host's
    /// two Protocol Control acknowledgements advance the response in the same
    /// batches observed in `xbox-full-startup.pcapng`.
    pub fn handle_host_packet(&mut self, packet: &[u8]) -> Vec<Vec<u8>> {
        if packet.len() < 4 {
            return Vec::new();
        }
        if packet[0] == frame::CMD_FIRMWARE {
            return self.handle_firmware_packet(packet);
        }
        let Some(parsed) = Packet::parse(packet) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        if parsed.wants_ack() {
            out.push(frame::ack(&parsed, parsed.payload.len() as u32, 0));
        }
        out.extend(self.handle_startup_packet(packet));
        out
    }

    fn handle_firmware_packet(&mut self, packet: &[u8]) -> Vec<Vec<u8>> {
        let Some(parsed) = Packet::parse(packet) else {
            return Vec::new();
        };
        let out = self.ufu.handle_packet(&parsed);
        for completed in self.ufu.device.take_completed() {
            // Component 3 is the application; it sets the reported version.
            if completed.component == 3 {
                self.firmware = completed.version;
            }
            self.completed.push(completed);
        }
        out
    }

    fn handle_startup_packet(&mut self, packet: &[u8]) -> Vec<Vec<u8>> {
        let payload_len = packet[3] as usize;
        if packet.len() < 4 + payload_len {
            return Vec::new();
        }

        match packet[0] {
            // Metadata Request: command 0x04 with an empty payload.
            0x04 if payload_len == 0 => {
                self.state = DeviceState::Metadata;
                self.metadata_ack_phase = Some(MetadataAckPhase::AwaitFirstAck);
                let mut first = METADATA_RESPONSE_PACKETS[0].to_vec();
                // The metadata lists the firmware major/minor it describes.
                first[45..49].copy_from_slice(&self.firmware.to_le()[..4]);
                vec![first]
            }
            // Protocol Control ACKs in the captured metadata exchange identify
            // the acknowledged message type (0x04) after the control code.
            0x01 if self.is_metadata_ack(packet, payload_len) => match self.metadata_ack_phase {
                Some(MetadataAckPhase::AwaitFirstAck) => {
                    self.metadata_ack_phase = Some(MetadataAckPhase::AwaitFinalAck);
                    METADATA_RESPONSE_PACKETS[1..6]
                        .iter()
                        .map(|frame| frame.to_vec())
                        .collect()
                }
                Some(MetadataAckPhase::AwaitFinalAck) => {
                    self.metadata_ack_phase = None;
                    self.state = DeviceState::Idle;
                    vec![METADATA_RESPONSE_PACKETS[6].to_vec()]
                }
                None => Vec::new(),
            },
            // Set Device State. The physical capture includes an additional
            // 15-byte vendor-specific form; only the standard 1-byte form is
            // interpreted here.
            0x05 if payload_len == 1 => {
                self.metadata_ack_phase = None;
                self.state = match packet[4] {
                    0x00 => DeviceState::Active,
                    0x01 => DeviceState::Idle,
                    0x04 => DeviceState::Off,
                    0x07 => {
                        self.hello_sequence = 0x02;
                        DeviceState::Arrival
                    }
                    _ => self.state,
                };
                // After a reset the Windows updater waits for a power-off
                // status, then a Hello with the same device ID.
                if packet[4] == 0x04 || packet[4] == 0x07 {
                    vec![self.powering_off_status()]
                } else {
                    Vec::new()
                }
            }
            _ => Vec::new(),
        }
    }

    fn is_metadata_ack(&self, packet: &[u8], payload_len: usize) -> bool {
        payload_len >= 2
            && packet.len() >= 6
            && packet[4] == 0x00
            && packet[5] == 0x04
            && self.metadata_ack_phase.is_some()
    }

    /// Report a wired, battery-free device powering off.
    /// A zero status is valid for this specific case; reserved/extended fields
    /// are zero and the status sequence is independent of Hello's sequence.
    fn powering_off_status(&mut self) -> Vec<u8> {
        let sequence = self.status_sequence;
        self.status_sequence = self.status_sequence.wrapping_add(1);
        if self.status_sequence == 0 {
            self.status_sequence = 1;
        }

        vec![0x03, 0x20, sequence, 0x04, 0x00, 0x00, 0x00, 0x00]
    }
}

/// `bcdDevice` as the physical controller reports it: major and minor in one
/// byte each (5.23 is `0x0517`).
pub fn bcd_device(firmware: Version) -> u16 {
    let [major, minor, ..] = firmware.0;
    (major & 0xff) << 8 | (minor & 0xff)
}

/// Parse a dotted firmware version such as `5.17.3202.0`.
pub fn parse_version(value: &str) -> Result<Version, String> {
    let parts = value
        .split('.')
        .map(|part| part.parse::<u16>())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| format!("invalid firmware version {value:?}"))?;
    let parts: [u16; 4] = parts
        .try_into()
        .map_err(|_| format!("firmware version {value:?} needs four parts"))?;
    Ok(Version(parts))
}

/// Build a USB UTF-16LE string descriptor for a USB gadget backend.
pub fn string_descriptor(value: &str) -> Vec<u8> {
    let units: Vec<u16> = value.encode_utf16().collect();
    let length = 2 + units.len() * 2;
    assert!(
        length <= u8::MAX as usize,
        "USB string descriptor is too long"
    );

    let mut descriptor = Vec::with_capacity(length);
    descriptor.push(length as u8);
    descriptor.push(0x03);
    for unit in units {
        descriptor.extend_from_slice(&unit.to_le_bytes());
    }
    descriptor
}

/// USB string descriptor zero advertises English (United States), required
/// for the GIP USB string descriptors.
pub const fn language_id_descriptor() -> [u8; 4] {
    [4, 0x03, 0x09, 0x04]
}

/// Build a FunctionFS v2 descriptor block for the captured controller
/// interfaces and alternate settings.
///
/// FunctionFS supplies the device and configuration descriptors through
/// configfs. Its descriptor block contains the interface/endpoint descriptors
/// for the function, in the same order at full and high speed. FunctionFS
/// requires one endpoint descriptor per endpoint file at each speed; the
/// captured interface 0 reuses endpoint addresses across alternate settings,
/// so the FunctionFS view assigns alternate setting 1 its own endpoint pair.
pub fn functionfs_descriptors() -> Vec<u8> {
    const DESCRIPTORS_MAGIC_V2: u32 = 3;
    const HAS_FS_DESC: u32 = 1;
    const HAS_HS_DESC: u32 = 2;

    let mut configuration = ConfigurationDescriptor::xbox_capture();
    for interface in &mut configuration.interfaces {
        if interface.number == 0 && interface.alternate_setting == 1 {
            interface.endpoints[0].address = 0x05;
            interface.endpoints[1].address = 0x85;
        }
    }
    let descriptor_count = configuration
        .interfaces
        .iter()
        .map(|interface| 1 + interface.endpoints.len())
        .sum::<usize>() as u32;
    let mut descriptors = Vec::new();
    for interface in &configuration.interfaces {
        interface.append_to(&mut descriptors);
    }
    let mut out = Vec::with_capacity(20 + descriptors.len() * 2);
    out.extend_from_slice(&DESCRIPTORS_MAGIC_V2.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes()); // filled with total length below
    out.extend_from_slice(&(HAS_FS_DESC | HAS_HS_DESC).to_le_bytes());
    out.extend_from_slice(&descriptor_count.to_le_bytes());
    out.extend_from_slice(&descriptor_count.to_le_bytes());
    out.extend_from_slice(&descriptors);
    out.extend_from_slice(&descriptors);
    let length = out.len() as u32;
    out[4..8].copy_from_slice(&length.to_le_bytes());
    out
}

/// Build the FunctionFS string table for the GIP interface.
pub fn functionfs_strings() -> Vec<u8> {
    let string = b"GIP Controller Interface\0";
    let mut out = Vec::with_capacity(18 + string.len());
    out.extend_from_slice(&2u32.to_le_bytes()); // FUNCTIONFS_STRINGS_MAGIC
    out.extend_from_slice(&((18 + string.len()) as u32).to_le_bytes());
    out.extend_from_slice(&1u32.to_le_bytes()); // string count
    out.extend_from_slice(&1u32.to_le_bytes()); // language count
    out.extend_from_slice(&0x0409u16.to_le_bytes());
    out.extend_from_slice(string);
    out
}

// Captured GIP Metadata Response fragments and Metadata Complete packet.
// The metadata blob supports firmware major/minor 5.23. Keep the Hello version
// in sync with this fixture unless the metadata is recompiled or updated.
const METADATA_RESPONSE_PACKETS: &[&[u8]] = &[
    &[
        0x04, 0xf0, 0x03, 0x3a, 0xa3, 0x02, 0x10, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x23, 0x01, 0xcd, 0x00, 0x16, 0x00, 0x1b, 0x00, 0x1c, 0x00,
        0x26, 0x00, 0x2f, 0x00, 0x4c, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01,
        0x05, 0x00, 0x17, 0x00, 0x00, 0x09, 0x01, 0x02, 0x03, 0x04, 0x06, 0x07, 0x0c, 0x0d, 0x1e,
        0x08, 0x01, 0x04, 0x05,
    ],
    &[
        0x04, 0xa0, 0x03, 0xba, 0x00, 0x3a, 0x06, 0x0a, 0x0c, 0x0d, 0x1e, 0x01, 0x1a, 0x00, 0x57,
        0x69, 0x6e, 0x64, 0x6f, 0x77, 0x73, 0x2e, 0x58, 0x62, 0x6f, 0x78, 0x2e, 0x49, 0x6e, 0x70,
        0x75, 0x74, 0x2e, 0x47, 0x61, 0x6d, 0x65, 0x70, 0x61, 0x64, 0x08, 0x56, 0xff, 0x76, 0x97,
        0xfd, 0x9b, 0x81, 0x45, 0xad, 0x45, 0xb6, 0x45, 0xbb, 0xa5, 0x26, 0xd6, 0x2c, 0x40, 0x2e,
        0x08, 0xdf, 0x07, 0xe1,
    ],
    &[
        0x04, 0xa0, 0x03, 0xba, 0x00, 0x74, 0x45, 0xa5, 0xab, 0xa3, 0x12, 0x7a, 0xf1, 0x97, 0xb5,
        0xe7, 0x1f, 0xf3, 0xb8, 0x86, 0x73, 0xe9, 0x40, 0xa9, 0xf8, 0x2f, 0x21, 0x26, 0x3a, 0xcf,
        0xb7, 0xfe, 0xd2, 0xdd, 0xec, 0x87, 0xd3, 0x94, 0x42, 0xbd, 0x96, 0x1a, 0x71, 0x2e, 0x3d,
        0xc7, 0x7d, 0x6b, 0xe5, 0xf2, 0x87, 0xbb, 0xc3, 0xb1, 0x49, 0x82, 0x65, 0xff, 0xff, 0xf3,
        0x77, 0x99, 0xee, 0x1e,
    ],
    &[
        0x04, 0xa0, 0x03, 0x3a, 0xae, 0x01, 0x9b, 0xad, 0x34, 0xad, 0x36, 0xb5, 0x4f, 0x8a, 0xc7,
        0x17, 0x23, 0x4c, 0x9f, 0x54, 0x6f, 0x77, 0xce, 0x34, 0x7a, 0xe2, 0x7d, 0xc6, 0x45, 0x8c,
        0xa4, 0x00, 0x42, 0xc0, 0x8b, 0xd9, 0x4a, 0xc0, 0xc8, 0x96, 0xea, 0x16, 0xb2, 0x8b, 0x44,
        0xbe, 0x80, 0x7e, 0x5d, 0xeb, 0x06, 0x98, 0xe2, 0x03, 0x17, 0x00, 0x20, 0x2c, 0x00, 0x01,
        0x00, 0x10, 0x00, 0x00,
    ],
    &[
        0x04, 0xa0, 0x03, 0x3a, 0xe8, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x17, 0x00, 0x09, 0x3c, 0x00, 0x01, 0x00, 0x08, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x17, 0x00, 0x1e,
        0x40, 0x00, 0x01, 0x00, 0x22, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00,
    ],
    &[0x04, 0xb0, 0x03, 0x01, 0xa2, 0x02, 0x00],
    &[0x04, 0xa0, 0x03, 0x00, 0xa3, 0x02],
];

pub const fn state() -> DeviceState {
    DeviceState::Arrival
}

#[cfg(test)]
mod tests {
    use super::*;
    use gip::rdhm::{Component, Package};
    use gip::updater::{Link, Options, Updater};
    use std::collections::VecDeque;

    struct ControllerLink {
        controller: VirtualController,
        queue: VecDeque<Vec<u8>>,
    }

    impl Link for ControllerLink {
        fn send(&mut self, packet: &[u8]) -> Result<(), String> {
            let out = self.controller.handle_host_packet(packet);
            self.queue.extend(out);
            if let Some(hello) = self.controller.hello_packet() {
                self.queue.push_back(hello.to_vec());
            }
            Ok(())
        }

        fn recv(&mut self, _timeout: Duration) -> Result<Option<Vec<u8>>, String> {
            Ok(self.queue.pop_front())
        }
    }

    #[test]
    fn host_updater_updates_virtual_controller() {
        let version = Version([5, 23, 6, 0]);
        let data: Vec<u8> = (0..0x2345u32).map(|i| (i * 7) as u8).collect();
        let package = Package {
            vendor_id: 0x045e,
            product_id: 0x0b12,
            version,
            components: vec![Component {
                id: 3,
                flags: 0,
                target_address: 0x1002_7000,
                version,
                data: data.clone(),
            }],
        };
        let mut link = ControllerLink {
            controller: VirtualController::with_identity(CAPTURED_DEVICE_ID, Version([5, 17, 3202, 0])),
            queue: VecDeque::new(),
        };
        let mut ignore = |_: &gip::updater::Event| {};
        Updater::new(&mut link, Options::default(), &mut ignore).run(&package).unwrap();

        assert_eq!(link.controller.firmware(), version);
        assert_eq!(link.controller.flash(0x1002_7000, data.len() as u32), Some(&data[..]));
        // The final reset returns the controller to Arrival with the new version.
        assert_eq!(link.controller.state(), DeviceState::Arrival);
        let hello = link.controller.hello_packet().unwrap();
        assert_eq!(&hello[16..24], &version.to_le());
        assert_eq!(bcd_device(version), 0x0517);
    }
}
