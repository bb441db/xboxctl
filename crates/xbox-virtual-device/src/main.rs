use std::{
    env,
    fs::OpenOptions,
    io::{Read, Write},
    path::Path,
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};
use xbox_virtual_device::{
    CAPTURED_DEVICE_ID, CAPTURED_FIRMWARE, HELLO_INTERVAL, VirtualController, parse_version,
};

const USAGE: &str =
    "Usage: xbox-virtual-device --functionfs <mount-path> [16-hex-digit-device-id] [--firmware <a.b.c.d>]";

fn main() {
    let mut args: Vec<String> = env::args().skip(1).collect();
    let firmware = match args.iter().position(|arg| arg == "--firmware") {
        Some(index) if index + 1 < args.len() => {
            let value = args.remove(index + 1);
            args.remove(index);
            parse_version(&value).unwrap_or_else(|error| {
                eprintln!("xbox-virtual-device: {error}");
                std::process::exit(2);
            })
        }
        Some(_) => {
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
        None => CAPTURED_FIRMWARE,
    };
    let mut args = args.into_iter();
    let mode = args.next();
    match (mode.as_deref(), args.next()) {
        (Some("--functionfs"), Some(path)) => {
            let device_id = match args.next() {
                Some(id) => match parse_device_id(&id) {
                    Ok(id) => id,
                    Err(error) => {
                        eprintln!("xbox-virtual-device: {error}");
                        std::process::exit(2);
                    }
                },
                None => CAPTURED_DEVICE_ID,
            };
            let controller = VirtualController::with_identity(device_id, firmware);
            if let Err(error) = run_functionfs(Path::new(&path), controller) {
                eprintln!("xbox-virtual-device: {error}");
                std::process::exit(1);
            }
        }
        (Some("--help"), _) => {
            println!("{USAGE}");
            println!("Run the GIP controller protocol over a mounted Linux FunctionFS gadget.");
            println!("--firmware sets the reported version (default {CAPTURED_FIRMWARE}).");
        }
        _ => {
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
    }
}

fn parse_device_id(value: &str) -> Result<[u8; 8], String> {
    if value.len() != 16 {
        return Err("device ID must contain exactly 16 hexadecimal digits".to_owned());
    }
    let mut id = [0u8; 8];
    for (index, byte) in id.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&value[index * 2..index * 2 + 2], 16)
            .map_err(|_| "device ID must contain only hexadecimal digits".to_owned())?;
    }
    Ok(id)
}

fn run_functionfs(mount_path: &Path, mut controller: VirtualController) -> Result<(), String> {
    let mut ep0 = OpenOptions::new()
        .read(true)
        .write(true)
        .open(mount_path.join("ep0"))
        .map_err(|error| format!("could not open FunctionFS ep0: {error}"))?;
    ep0.write_all(&xbox_virtual_device::functionfs_descriptors())
        .map_err(|error| format!("could not register FunctionFS descriptors: {error}"))?;
    ep0.write_all(&xbox_virtual_device::functionfs_strings())
        .map_err(|error| format!("could not register FunctionFS strings: {error}"))?;

    // FunctionFS creates one endpoint file for every endpoint descriptor,
    // including endpoints in alternate settings. The first two descriptors
    // are the GIP interrupt OUT/IN endpoints; keep the remaining files open
    // so the captured isochronous and bulk interfaces are available too.
    let mut endpoint_files = (1..=8)
        .map(|number| {
            OpenOptions::new()
                .read(true)
                .write(true)
                .open(mount_path.join(format!("ep{number}")))
                .map_err(|error| format!("could not open FunctionFS endpoint ep{number}: {error}"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let out_endpoint = endpoint_files.remove(0);
    let mut in_endpoint = endpoint_files.remove(0);
    let _other_endpoint_files = endpoint_files;

    let (host_tx, host_rx) = mpsc::channel::<Vec<u8>>();
    thread::Builder::new()
        .name("gip-out".to_owned())
        .spawn(move || read_host_packets(out_endpoint, host_tx))
        .map_err(|error| format!("could not start GIP OUT reader: {error}"))?;
    thread::Builder::new()
        .name("functionfs-control".to_owned())
        .spawn(move || handle_control_events(ep0))
        .map_err(|error| format!("could not start FunctionFS control reader: {error}"))?;

    eprintln!(
        "GIP virtual controller ready: {:04x}:{:04x}, device ID {}, firmware {}, state {:?}",
        xbox_virtual_device::USB_IDENTITY.vendor_id,
        xbox_virtual_device::USB_IDENTITY.product_id,
        controller.serial_number(),
        controller.firmware(),
        controller.state()
    );
    let mut next_hello = Instant::now();

    loop {
        let now = Instant::now();
        if now >= next_hello {
            if let Some(hello) = controller.hello_packet() {
                if let Err(error) = write_packet(&mut in_endpoint, &hello) {
                    eprintln!("GIP Hello send failed (waiting for USB/IP host): {error}");
                } else {
                    log_packet("IN", &hello);
                }
            }
            next_hello = Instant::now() + HELLO_INTERVAL;
        }

        let wait = next_hello.saturating_duration_since(Instant::now());
        match host_rx.recv_timeout(wait) {
            Ok(packet) => {
                if !is_bulk_firmware_packet(&packet) {
                    log_packet("OUT", &packet);
                }
                let previous_state = controller.state();
                let responses = controller.handle_host_packet(&packet);
                for completed in controller.take_completed() {
                    eprintln!(
                        "Firmware component {} written: {} bytes at {:#010x}, version {}",
                        completed.component, completed.size, completed.address, completed.version
                    );
                }
                if controller.state() != previous_state {
                    eprintln!(
                        "GIP state: {:?} -> {:?}",
                        previous_state,
                        controller.state()
                    );
                }
                for response in responses {
                    write_packet(&mut in_endpoint, &response)?;
                    if !is_bulk_firmware_packet(&response) {
                        log_packet("IN", &response);
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err("GIP OUT reader stopped".to_owned());
            }
        }
    }
}

fn read_host_packets(mut endpoint: std::fs::File, sender: mpsc::Sender<Vec<u8>>) {
    let mut buffer = [0u8; 1024];
    loop {
        match endpoint.read(&mut buffer) {
            Ok(0) => thread::sleep(Duration::from_millis(20)),
            Ok(length) => {
                if sender.send(buffer[..length].to_vec()).is_err() {
                    return;
                }
            }
            Err(error) => {
                eprintln!("GIP OUT endpoint read failed: {error}");
                thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

fn handle_control_events(mut ep0: std::fs::File) {
    let mut event = [0u8; 12];
    loop {
        if let Err(error) = ep0.read_exact(&mut event) {
            eprintln!("FunctionFS ep0 event read failed: {error}");
            thread::sleep(Duration::from_millis(100));
            continue;
        }
        match event[8] {
            0 => eprintln!("FunctionFS: BIND"),
            1 => eprintln!("FunctionFS: UNBIND"),
            2 => eprintln!("FunctionFS: ENABLE"),
            3 => eprintln!("FunctionFS: DISABLE"),
            4 => {
                let value = u16::from_le_bytes([event[2], event[3]]);
                let index = u16::from_le_bytes([event[4], event[5]]);
                let length = u16::from_le_bytes([event[6], event[7]]);
                eprintln!(
                    "FunctionFS: SETUP bmRequestType=0x{:02x} bRequest=0x{:02x} wValue=0x{value:04x} wIndex=0x{index:04x} wLength={length}",
                    event[0], event[1]
                );
                // Standard enumeration requests are handled by configfs and
                // FunctionFS. Unknown function-specific control requests are
                // stalled so Windows can continue probing other descriptors.
                match (event[0], event[1]) {
                    (0x81, 0x0a) => {
                        if let Err(error) = ep0.write_all(&[0]) {
                            eprintln!("FunctionFS GET_INTERFACE reply failed: {error}");
                        }
                    }
                    (0x01, 0x0b) => {
                        if let Err(error) = ep0.write(&[]) {
                            eprintln!("FunctionFS SET_INTERFACE reply failed: {error}");
                        }
                    }
                    _ => {
                        if let Err(error) = ep0.write(&[]) {
                            eprintln!("FunctionFS setup stall failed: {error}");
                        }
                    }
                }
            }
            5 => eprintln!("FunctionFS: SUSPEND"),
            6 => eprintln!("FunctionFS: RESUME"),
            other => eprintln!("FunctionFS: unknown event {other}"),
        }
    }
}

fn write_packet(endpoint: &mut std::fs::File, packet: &[u8]) -> Result<(), String> {
    endpoint
        .write_all(packet)
        .map_err(|error| format!("GIP IN endpoint write failed: {error}"))
}

/// Firmware data fragments and their ACKs; logging them would bury
/// everything else during an update.
fn is_bulk_firmware_packet(packet: &[u8]) -> bool {
    let fragment = packet.len() >= 2 && packet[0] == 0x0c && packet[1] & 0xc0 == 0x80;
    let ack = packet.len() >= 6 && packet[0] == 0x01 && packet[5] == 0x0c;
    fragment || ack
}

fn log_packet(direction: &str, packet: &[u8]) {
    let bytes = packet
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<Vec<_>>()
        .join(" ");
    eprintln!("GIP {direction} ({} bytes): {bytes}", packet.len());
}
