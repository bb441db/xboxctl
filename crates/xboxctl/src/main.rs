mod usb;

use clap::{Parser, Subcommand};
use gip::{
    rdhm::Package,
    updater::{Event, Link, Options, Updater},
};
use std::{
    fs,
    io::{self, BufRead, Write},
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};
use usb::UsbLink;

#[derive(Debug, Parser)]
#[command(name = "xboxctl", about = "Inspect and research Xbox USB controllers")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Read the firmware version from a connected controller or a USBPcap capture.
    Firmware {
        /// Capture file to inspect (pcapng with USBPcap link type); omit for live USB.
        #[arg(long)]
        capture: Option<PathBuf>,
        /// How long to listen for the startup Hello message during a live read.
        #[arg(long, default_value_t = 30)]
        timeout_seconds: u64,
        /// Print interrupt-IN and interrupt-OUT payloads as hexadecimal during live USB.
        #[arg(long)]
        trace_packets: bool,
    },
    /// Update controller firmware from an rdhm package (the Xbox Accessories
    /// app's MERL_*.bin files). Tested against emulated controllers only.
    Update {
        /// rdhm firmware package.
        image: PathBuf,
        /// Validate the package and show what would be written, without USB.
        #[arg(long)]
        dry_run: bool,
        /// Skip the confirmation prompt.
        #[arg(long)]
        yes: bool,
        /// Print every GIP packet.
        #[arg(long)]
        trace_packets: bool,
    },
    /// Report the current controller connection status.
    Status,
}

fn main() {
    let cli = Cli::parse();
    match cli.command {
        Command::Firmware {
            capture,
            timeout_seconds,
            trace_packets,
        } => {
            let reading_capture = capture.is_some();
            let result = match capture {
                Some(path) => read_hello_from_capture(&path)
                    .map_err(|error| format!("Could not read {}: {error}", path.display())),
                None => read_live_hello(Duration::from_secs(timeout_seconds), trace_packets),
            };
            match result {
                Ok(Some(info)) => print_firmware_info(&info),
                Ok(None) => {
                    if reading_capture {
                        eprintln!("No GIP Hello Device message found in the capture.");
                    } else {
                        eprintln!(
                            "No GIP Hello Device message arrived before the timeout. Confirm the controller is connected to this Linux host; if its startup exchange has already completed, reconnect it and try again."
                        );
                    }
                    std::process::exit(1);
                }
                Err(error) => {
                    eprintln!("{error}");
                    std::process::exit(2);
                }
            }
        }
        Command::Update { image, dry_run, yes, trace_packets } => {
            if let Err(error) = update(&image, dry_run, yes, trace_packets) {
                eprintln!("Update failed: {error}");
                std::process::exit(1);
            }
        }
        Command::Status => {
            eprintln!("Controller status inspection is not implemented yet.");
            std::process::exit(2);
        }
    }
}

fn update(image: &Path, dry_run: bool, yes: bool, trace: bool) -> Result<(), String> {
    let bytes = fs::read(image).map_err(|error| format!("could not read {}: {error}", image.display()))?;
    let package = Package::parse(&bytes)?;
    println!(
        "Package {:04x}:{:04x} firmware {}",
        package.vendor_id, package.product_id, package.version
    );
    for component in &package.components {
        println!(
            "  component {} version {:<12} {:#x} bytes at {:#010x}{}",
            component.id,
            component.version.to_string(),
            component.data.len(),
            component.target_address,
            if component.is_critical() { "  (critical: not written)" } else { "" }
        );
    }
    if dry_run {
        return Ok(());
    }
    if !yes {
        println!(
            "\nThis writes firmware to the controller. The update protocol has only been \
             tested against emulated controllers; an interrupted or rejected update may leave \
             a physical controller unusable. Keep it connected until the update finishes."
        );
        print!("Type 'update' to continue: ");
        io::stdout().flush().ok();
        let mut answer = String::new();
        io::stdin().lock().read_line(&mut answer).map_err(|error| error.to_string())?;
        if answer.trim() != "update" {
            return Err("cancelled".to_owned());
        }
    }

    let mut link = UsbLink::open(package.vendor_id, package.product_id, Duration::from_secs(10), trace)?;
    if let Some(info) = start_device(&mut link)? {
        println!("Controller reports firmware {}.{}.{}.{}", info.major, info.minor, info.build, info.revision);
    }

    let mut last_percent = None;
    let mut report = |event: &Event| match event {
        Event::Order(order) => println!("Update order: {order:?}"),
        Event::DeviceVersion { component, version } => {
            println!("  device component {component}: {version}")
        }
        Event::UpToDate { component } => println!("Component {component} is up to date"),
        Event::Begin { component, version, size } => {
            last_percent = None;
            println!("Writing component {component} ({version}, {size:#x} bytes)");
        }
        Event::Block { done, total, .. } => {
            let percent = u64::from(*done) * 100 / u64::from(*total) / 10 * 10;
            if last_percent != Some(percent) {
                last_percent = Some(percent);
                println!("  {percent}%");
            }
        }
        Event::Complete { component } => println!("Component {component} complete"),
        Event::Skipped { component, reason } => println!("Component {component} skipped: {reason}"),
        Event::Reset => println!("Resetting controller"),
        Event::Retry { op, attempt } => {
            eprintln!("  no response to {}, attempt {attempt}", gip::ufu::op_name(*op))
        }
        Event::FragmentResend { offset } => eprintln!("  resending from byte {offset}"),
        Event::Info(_) => {}
    };
    Updater::new(&mut link, Options::default(), &mut report).run(&package)?;
    drop(link);

    println!("Update finished; waiting for the controller to restart");
    thread::sleep(Duration::from_secs(1));
    match UsbLink::open(package.vendor_id, package.product_id, Duration::from_secs(20), trace)
        .and_then(|mut link| start_device(&mut link))
    {
        Ok(Some(info)) => println!(
            "Controller now reports firmware {}.{}.{}.{}",
            info.major, info.minor, info.build, info.revision
        ),
        Ok(None) => println!("Controller restarted but did not send Hello"),
        Err(error) => println!("Could not reopen the controller: {error}"),
    }
    Ok(())
}

/// Send Power On and wait briefly for the controller's Hello.
fn start_device(link: &mut UsbLink) -> Result<Option<FirmwareInfo>, String> {
    link.send(&[0x05, 0x20, 0x00, 0x01, 0x00])?;
    let deadline = Instant::now() + Duration::from_secs(2);
    while let Some(packet) = link.recv(deadline.saturating_duration_since(Instant::now()))? {
        if let Some(info) = parse_gip_hello(&packet) {
            return Ok(Some(info));
        }
    }
    Ok(None)
}

fn print_firmware_info(info: &FirmwareInfo) {
    println!(
        "USB {:04x}:{:04x}\nFirmware {}.{}.{}.{}",
        info.vendor_id, info.product_id, info.major, info.minor, info.build, info.revision
    );
}

/// Start the GIP exchange and listen for the controller's startup Hello.
fn read_live_hello(timeout: Duration, trace_packets: bool) -> Result<Option<FirmwareInfo>, String> {
    const VENDOR_ID: u16 = 0x045e;
    const PRODUCT_ID: u16 = 0x0b12;
    const INTERFACE: u8 = 0;
    const ENDPOINT_IN: u8 = 0x82;
    const ENDPOINT_OUT: u8 = 0x02;
    const POWER_ON: &[u8] = &[0x05, 0x20, 0x00, 0x01, 0x00];
    const LED: &[u8] = &[0x0a, 0x20, 0x01, 0x03, 0x00, 0x01, 0x14];
    const AUTHENTICATE: &[u8] = &[0x06, 0x20, 0x02, 0x02, 0x01, 0x00];

    eprintln!(
        "Starting the GIP exchange on endpoint 0x{ENDPOINT_OUT:02x}, then listening for Hello on 0x{ENDPOINT_IN:02x}."
    );
    let deadline = Instant::now() + timeout;
    let handle = loop {
        if Instant::now() >= deadline {
            return Ok(None);
        }
        let Some(handle) = rusb::open_device_with_vid_pid(VENDOR_ID, PRODUCT_ID) else {
            thread::sleep(Duration::from_millis(200));
            continue;
        };
        match handle.claim_interface(INTERFACE) {
            Ok(()) => {}
            Err(rusb::Error::NoDevice) => continue,
            Err(error) => {
                return Err(format!(
                    "Could not claim controller USB interface {INTERFACE}: {error}. Close other programs using the controller and check USB permissions."
                ));
            }
        }
        break handle;
    };

    // Match xpad's ordering: keep an interrupt-IN transfer pending before
    // waking the controller with Power On, since Hello can follow immediately.
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let (hello_tx, hello_rx) = std::sync::mpsc::channel();
    thread::scope(|scope| {
        scope.spawn(|| {
            let _ = ready_tx.send(());
            let mut buffer = [0u8; 64];
            let result = loop {
                if Instant::now() >= deadline {
                    break Ok(None);
                }
                let remaining = deadline.saturating_duration_since(Instant::now());
                match handle.read_interrupt(
                    ENDPOINT_IN,
                    &mut buffer,
                    remaining.min(Duration::from_millis(500)),
                ) {
                    Ok(length) => {
                        if trace_packets {
                            let hex = buffer[..length]
                                .iter()
                                .map(|byte| format!("{byte:02x}"))
                                .collect::<Vec<_>>()
                                .join(" ");
                            eprintln!("interrupt IN ({length} bytes): {hex}");
                        }
                        if let Some(info) = parse_gip_hello(&buffer[..length]) {
                            break Ok(Some(info));
                        }
                    }
                    Err(rusb::Error::Timeout) => continue,
                    Err(error) => {
                        break Err(format!(
                            "USB read from endpoint 0x{ENDPOINT_IN:02x} failed: {error}"
                        ));
                    }
                }
            };
            let _ = hello_tx.send(result);
        });

        if ready_rx.recv().is_err() {
            return Err("USB reader thread could not start".to_owned());
        }
        // Give libusb time to submit the first interrupt-IN transfer.
        thread::sleep(Duration::from_millis(20));
        send_gip_packet(&handle, ENDPOINT_OUT, POWER_ON, trace_packets)?;
        send_gip_packet(&handle, ENDPOINT_OUT, LED, trace_packets)?;
        send_gip_packet(&handle, ENDPOINT_OUT, AUTHENTICATE, trace_packets)?;

        let remaining = deadline.saturating_duration_since(Instant::now());
        let info = match hello_rx.recv_timeout(remaining) {
            Ok(result) => result?,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => None,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                return Err("USB reader stopped before returning a result".to_owned());
            }
        };
        Ok(info)
    })
}

fn send_gip_packet(
    handle: &rusb::DeviceHandle<rusb::GlobalContext>,
    endpoint: u8,
    packet: &[u8],
    trace_packets: bool,
) -> Result<(), String> {
    let transferred = handle
        .write_interrupt(endpoint, packet, Duration::from_secs(1))
        .map_err(|error| format!("USB write to endpoint 0x{endpoint:02x} failed: {error}"))?;
    if transferred != packet.len() {
        return Err(format!(
            "USB write to endpoint 0x{endpoint:02x} was short: {transferred}/{} bytes",
            packet.len()
        ));
    }
    if trace_packets {
        let hex = packet
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<Vec<_>>()
            .join(" ");
        eprintln!("interrupt OUT ({} bytes): {hex}", packet.len());
    }
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
struct FirmwareInfo {
    vendor_id: u16,
    product_id: u16,
    major: u16,
    minor: u16,
    build: u16,
    revision: u16,
}

/// Find a GIP Hello Device message in a pcapng USBPcap capture.
fn read_hello_from_capture(path: &PathBuf) -> Result<Option<FirmwareInfo>, String> {
    let bytes = fs::read(path).map_err(|error| error.to_string())?;
    parse_pcapng_hello(&bytes)
}

fn parse_pcapng_hello(bytes: &[u8]) -> Result<Option<FirmwareInfo>, String> {
    const SECTION_HEADER: u32 = 0x0a0d_0d0a;
    const INTERFACE_DESCRIPTION: u32 = 1;
    const ENHANCED_PACKET: u32 = 6;
    const USBPCAP_LINKTYPE: u16 = 249;

    let mut offset = 0usize;
    let mut little_endian = true;
    let mut link_types = Vec::<u16>::new();

    while offset + 12 <= bytes.len() {
        let raw_type = u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap());
        if raw_type == SECTION_HEADER {
            let bom = bytes
                .get(offset + 8..offset + 12)
                .ok_or_else(|| "truncated pcapng section header".to_owned())?;
            little_endian = match bom {
                [0x4d, 0x3c, 0x2b, 0x1a] => true,
                [0x1a, 0x2b, 0x3c, 0x4d] => false,
                _ => return Err("invalid pcapng byte-order magic".to_owned()),
            };
            link_types.clear();
        }

        let block_type = read_u32(bytes, offset, little_endian)?;
        let block_len = read_u32(bytes, offset + 4, little_endian)? as usize;
        if block_len < 12
            || offset
                .checked_add(block_len)
                .is_none_or(|end| end > bytes.len())
        {
            return Err(format!(
                "invalid pcapng block length {block_len} at {offset}"
            ));
        }

        match block_type {
            SECTION_HEADER => {}
            INTERFACE_DESCRIPTION => {
                if block_len < 20 {
                    return Err("truncated pcapng interface description".to_owned());
                }
                link_types.push(read_u16(bytes, offset + 8, little_endian)?);
            }
            ENHANCED_PACKET => {
                if block_len < 32 {
                    return Err("truncated pcapng enhanced packet".to_owned());
                }
                let interface_id = read_u32(bytes, offset + 8, little_endian)? as usize;
                let captured_len = read_u32(bytes, offset + 20, little_endian)? as usize;
                let packet_start = offset + 28;
                let packet_end = packet_start
                    .checked_add(captured_len)
                    .ok_or_else(|| "pcapng packet length overflow".to_owned())?;
                let block_end = offset + block_len - 4;
                if packet_end > block_end {
                    return Err("pcapng packet exceeds its block".to_owned());
                }
                if link_types.get(interface_id) == Some(&USBPCAP_LINKTYPE) {
                    if let Some(info) = parse_usbpcap_hello(&bytes[packet_start..packet_end])? {
                        return Ok(Some(info));
                    }
                }
            }
            _ => {}
        }
        offset += block_len;
    }

    if offset != bytes.len() {
        return Err("trailing or truncated pcapng data".to_owned());
    }
    Ok(None)
}

fn parse_usbpcap_hello(packet: &[u8]) -> Result<Option<FirmwareInfo>, String> {
    if packet.is_empty() {
        return Ok(None);
    }
    let header_len = packet[0] as usize;
    let gip = packet
        .get(header_len..)
        .ok_or_else(|| "invalid USBPcap header length".to_owned())?;
    Ok(parse_gip_hello(gip))
}

fn parse_gip_hello(gip: &[u8]) -> Option<FirmwareInfo> {
    if gip.len() < 32 || gip[0] != 0x02 || gip[1] != 0x20 || gip[3] != 0x1c {
        return None;
    }

    Some(FirmwareInfo {
        vendor_id: u16::from_le_bytes([gip[12], gip[13]]),
        product_id: u16::from_le_bytes([gip[14], gip[15]]),
        major: u16::from_le_bytes([gip[16], gip[17]]),
        minor: u16::from_le_bytes([gip[18], gip[19]]),
        build: u16::from_le_bytes([gip[20], gip[21]]),
        revision: u16::from_le_bytes([gip[22], gip[23]]),
    })
}

fn read_u16(bytes: &[u8], offset: usize, little_endian: bool) -> Result<u16, String> {
    let data: [u8; 2] = bytes
        .get(offset..offset + 2)
        .ok_or_else(|| "truncated pcapng field".to_owned())?
        .try_into()
        .unwrap();
    Ok(if little_endian {
        u16::from_le_bytes(data)
    } else {
        u16::from_be_bytes(data)
    })
}

fn read_u32(bytes: &[u8], offset: usize, little_endian: bool) -> Result<u32, String> {
    let data: [u8; 4] = bytes
        .get(offset..offset + 4)
        .ok_or_else(|| "truncated pcapng field".to_owned())?
        .try_into()
        .unwrap();
    Ok(if little_endian {
        u32::from_le_bytes(data)
    } else {
        u32::from_be_bytes(data)
    })
}
