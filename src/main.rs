mod calibrate;
mod diag;
mod feed;
mod gip;
mod holders;
#[cfg(target_os = "macos")]
mod iokit;
mod live;
#[cfg(unix)]
mod sys;
mod usb;

use crate::gip::{
    frame::{Hello, Packet},
    rdhm::{Package, Version},
    updater::{Event, Link, Options, Updater},
};
use clap::{Parser, Subcommand};
use std::{
    fs,
    io::{self, BufRead, IsTerminal, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use usb::{Controller, UsbLink};

#[derive(Debug, Parser)]
#[command(
    name = "xboxctl",
    version,
    about = "Inspect and update the firmware of Xbox (GIP) controllers"
)]
struct Cli {
    /// Print every GIP packet to stderr (debug builds only).
    #[cfg(debug_assertions)]
    #[arg(long, global = true)]
    trace_packets: bool,
    #[command(subcommand)]
    command: Command,
}

impl Cli {
    /// Packet tracing exists only in debug builds.
    fn trace_packets(&self) -> bool {
        #[cfg(debug_assertions)]
        return self.trace_packets;
        #[cfg(not(debug_assertions))]
        return false;
    }
}

#[derive(Debug, Subcommand)]
enum Command {
    /// List connected Xbox (GIP) controllers.
    List,
    /// Show a controller's identity and firmware versions. Read-only.
    Info {
        /// Controller to use, as BUS:ADDRESS from `xboxctl list`.
        #[arg(long)]
        device: Option<String>,
    },
    /// Download the current firmware for a controller from Microsoft's
    /// firmware feed (the one the Xbox Accessories app uses).
    Download {
        /// Controller to download for, as BUS:ADDRESS from `xboxctl list`.
        #[arg(long, conflicts_with = "id")]
        device: Option<String>,
        /// Download for this VID:PID instead of a connected controller.
        #[arg(long)]
        id: Option<String>,
        /// Directory for the firmware file (default: the user cache directory).
        #[arg(long)]
        output: Option<PathBuf>,
        /// List every controller in the feed instead of downloading.
        #[arg(long)]
        list: bool,
    },
    /// Update controller firmware from an rdhm package (the Xbox Accessories
    /// app's MERL_*.bin files) or the current firmware from Microsoft's feed.
    Update {
        /// rdhm firmware package.
        #[arg(required_unless_present = "latest")]
        image: Option<PathBuf>,
        /// Download the current firmware from Microsoft's feed and install it.
        #[arg(long, conflicts_with = "image")]
        latest: bool,
        /// Controller to update, as BUS:ADDRESS from `xboxctl list`.
        #[arg(long)]
        device: Option<String>,
        /// Show the package and what would change, without writing anything.
        #[arg(long)]
        dry_run: bool,
        /// Skip the confirmation prompt.
        #[arg(long)]
        yes: bool,
        /// Only update these component ids (comma separated); the rest of
        /// the package is ignored, including its size checks.
        #[arg(long, value_delimiter = ',')]
        only: Vec<u32>,
        /// Rewrite the selected components even if their version matches.
        /// On MERL, components 4 and 3 share flash: force them together
        /// (--only 4,3), never 4 alone.
        #[arg(long, requires = "only")]
        force: bool,
        /// Hash ADDRESS:LENGTH right after BeginUpdate, before writing
        /// (repeatable). Diagnostic; needs --only.
        #[arg(long, requires = "only", value_parser = parse_range)]
        probe: Vec<(u32, u32)>,
    },
    /// Back up a range of flash that UFU cannot read directly, by rebuilding
    /// it from 2-byte GetBlockHash results. The device only answers those
    /// during an update, so this reinstalls components 4 and 3 from the
    /// latest firmware around it; the controller ends on the same firmware.
    Backup {
        /// File to write; it must not exist yet.
        output: PathBuf,
        /// Controller to use, as BUS:ADDRESS from `xboxctl list`.
        #[arg(long)]
        device: Option<String>,
        /// Start address (default: component 2 on MERL).
        #[arg(long, value_parser = parse_u32, default_value = "0x10005000")]
        address: u32,
        /// Number of bytes (default: component 2 plus the 4 KiB after it).
        #[arg(long, value_parser = parse_u32, default_value = "0x22000")]
        length: u32,
        /// Skip the confirmation prompt.
        #[arg(long)]
        yes: bool,
    },
    /// Run each of the controller's rumble motors for a moment, to tell a
    /// controller fault from a driver problem. Changes nothing on the
    /// controller. On macOS, run with sudo and see Permissions in the README.
    Rumble {
        /// Controller to use, as BUS:ADDRESS from `xboxctl list`.
        #[arg(long)]
        device: Option<String>,
        /// Motor strength, 1 to 100.
        #[arg(long, default_value_t = 50, value_parser = clap::value_parser!(u8).range(1..=100))]
        strength: u8,
    },
    /// Recalibrate the controller's thumbsticks or triggers with a guided
    /// walkthrough, and check them afterwards. The controller forgets its
    /// current calibration as soon as you start, so finish all the steps.
    Calibrate {
        /// Controller to use, as BUS:ADDRESS from `xboxctl list`.
        #[arg(long)]
        device: Option<String>,
        /// Which stick to calibrate; both are done one after the other. The
        /// sticks are the default when neither this nor --trigger is given.
        #[arg(long, value_enum)]
        stick: Option<calibrate::Side>,
        /// Which trigger to calibrate; both are done one after the other.
        #[arg(long, value_enum)]
        trigger: Option<calibrate::Side>,
        /// Show the current calibration without changing anything.
        #[arg(long)]
        dry_run: bool,
        /// Skip the confirmation prompt.
        #[arg(long)]
        yes: bool,
    },
    /// Show what the controller reports about itself (identity, firmware),
    /// then a live view of its buttons, sticks and triggers that
    /// also watches for stick drift. Changes nothing on the controller.
    Diag {
        /// Controller to use, as BUS:ADDRESS from `xboxctl list`.
        #[arg(long)]
        device: Option<String>,
        /// Stop the live view after this many seconds (default: until you
        /// hold the Xbox button for two seconds, or press Ctrl-C).
        #[arg(long, value_name = "SECONDS", value_parser = clap::value_parser!(u64).range(1..))]
        duration: Option<u64>,
    },
    /// Debugging aids (debug builds only).
    #[cfg(debug_assertions)]
    Test {
        #[command(subcommand)]
        action: TestAction,
    },
}

#[cfg(debug_assertions)]
#[derive(Debug, Subcommand)]
enum TestAction {
    /// Reset the controller, wait for it to restart and re-enumerate, answer
    /// its Hello and check that it responds. Writes nothing.
    Reset {
        /// Controller to use, as BUS:ADDRESS from `xboxctl list`.
        #[arg(long)]
        device: Option<String>,
    },
}

fn main() {
    let cli = Cli::parse();
    let trace_packets = cli.trace_packets();
    match cli.command {
        Command::List => {
            if let Err(error) = list() {
                eprintln!("{error}");
                std::process::exit(1);
            }
        }
        Command::Info { device } => {
            if let Err(error) = info(device.as_deref(), trace_packets) {
                eprintln!("{error}");
                std::process::exit(1);
            }
        }
        Command::Backup {
            output,
            device,
            address,
            length,
            yes,
        } => {
            let extra = UpdateExtras {
                only: vec![4, 3],
                force: true,
                probe: Vec::new(),
                backup: Some((address, length, output)),
            };
            if let Err(error) = update(None, device.as_deref(), false, yes, extra, trace_packets) {
                eprintln!("Backup failed: {error}");
                std::process::exit(1);
            }
        }
        Command::Download {
            device,
            id,
            output,
            list,
        } => {
            let result = if list {
                list_feed()
            } else {
                download(device.as_deref(), id.as_deref(), output).map(|_| ())
            };
            if let Err(error) = result {
                eprintln!("{error}");
                std::process::exit(1);
            }
        }
        Command::Update {
            image,
            latest,
            device,
            dry_run,
            yes,
            only,
            force,
            probe,
        } => {
            let source = if latest { None } else { image.as_deref() };
            let extra = UpdateExtras {
                only,
                force,
                probe,
                backup: None,
            };
            if let Err(error) =
                update(source, device.as_deref(), dry_run, yes, extra, trace_packets)
            {
                eprintln!("Update failed: {error}");
                std::process::exit(1);
            }
        }
        Command::Rumble { device, strength } => {
            if let Err(error) = rumble(device.as_deref(), strength, trace_packets) {
                eprintln!("Rumble test failed: {error}");
                std::process::exit(1);
            }
        }
        Command::Calibrate {
            device,
            stick,
            trigger,
            dry_run,
            yes,
        } => {
            if let Err(error) = calibrate::run(
                device.as_deref(),
                stick,
                trigger,
                dry_run,
                yes,
                trace_packets,
            ) {
                eprintln!("{error}");
                std::process::exit(1);
            }
        }
        Command::Diag { device, duration } => {
            if let Err(error) = diag::run(
                device.as_deref(),
                duration.map(Duration::from_secs),
                trace_packets,
            ) {
                eprintln!("{error}");
                std::process::exit(1);
            }
        }
        #[cfg(debug_assertions)]
        Command::Test {
            action: TestAction::Reset { device },
        } => {
            if let Err(error) = test_reset(device.as_deref(), trace_packets) {
                eprintln!("Reset test failed: {error}");
                std::process::exit(1);
            }
        }
    }
}

/// Run the handle motors, then the trigger motors, for a second each. The
/// motors are always stopped again, also when a step fails.
fn rumble(device: Option<&str>, strength: u8, trace: bool) -> Result<(), String> {
    use crate::gip::{
        frame::Sequence,
        rumble::{Motors, packet, resync},
    };

    let controller = usb::select_controller(device)?;
    println!("Rumbling {}", describe(&controller));
    let mut link = UsbLink::open(&controller, trace)?;
    start_device(&mut link)?;

    let mut sequence = Sequence::default();
    // The controller ignores rumble whose sequence number is not ahead of the
    // last one it saw, including one from an earlier run.
    for packet in resync(&mut sequence) {
        link.send(&packet)?;
        std::thread::sleep(Duration::from_millis(10));
    }
    let steps = [
        (
            "handles",
            Motors {
                left_handle: strength,
                right_handle: strength,
                ..Motors::OFF
            },
        ),
        (
            "triggers",
            Motors {
                left_trigger: strength,
                right_trigger: strength,
                ..Motors::OFF
            },
        ),
    ];
    let mut run = |link: &mut UsbLink| -> Result<(), String> {
        for (name, motors) in steps {
            println!("  {name} at {strength}%");
            link.send(&packet(sequence.take(), motors))?;
            std::thread::sleep(Duration::from_secs(1));
            link.send(&packet(sequence.take(), Motors::OFF))?;
            std::thread::sleep(Duration::from_millis(300));
        }
        Ok(())
    };
    let result = run(&mut link);
    // After a failure the first stop may have been skipped; stop again.
    let stopped = link.send(&packet(sequence.take(), Motors::OFF));
    result.and(stopped)?;
    println!("Done. If nothing moved, the controller itself may be at fault.");
    Ok(())
}

/// Exercise the restart an update goes through, without writing anything:
/// reset, wait for re-enumeration, answer the Hello, check the controller
/// responds. Prints how long each step took.
#[cfg(debug_assertions)]
fn test_reset(device: Option<&str>, trace: bool) -> Result<(), String> {
    use crate::gip::frame;

    let controller = usb::select_controller(device)?;
    println!("Resetting {}", describe(&controller));
    let mut link = UsbLink::open(&controller, trace)?;
    start_device(&mut link)?;

    let start = Instant::now();
    let ms = |since: Instant| since.elapsed().as_millis();
    link.send(&frame::packet(
        frame::CMD_SET_DEVICE_STATE,
        frame::FLAG_SYSTEM,
        0x02,
        &[frame::STATE_RESET],
    ))?;

    // Power-off status (bits 6-7 clear). The link may also drop first.
    let deadline = start + Duration::from_secs(5);
    loop {
        match link.recv(deadline.saturating_duration_since(Instant::now())) {
            Ok(Some(bytes)) => {
                let Some(packet) = Packet::parse(&bytes) else {
                    continue;
                };
                if packet.command == frame::CMD_STATUS
                    && packet
                        .payload
                        .first()
                        .is_some_and(|status| status & 0xc0 == 0)
                {
                    println!("  power-off status after {} ms", ms(start));
                    break;
                }
            }
            Ok(None) => {
                println!("  no power-off status within 5 s");
                break;
            }
            Err(error) => {
                println!("  link dropped after {} ms ({error})", ms(start));
                break;
            }
        }
    }

    link.reconnect(Duration::from_secs(30))?;
    println!("  reopened after {} ms", ms(start));

    let reopened = Instant::now();
    let deadline = reopened + Duration::from_secs(1);
    let mut hello = None;
    while let Some(bytes) = link.recv(deadline.saturating_duration_since(Instant::now()))? {
        if let Some(found) = Packet::parse(&bytes).as_ref().and_then(Hello::parse) {
            hello = Some(found);
            break;
        }
    }
    match hello {
        Some(hello) => println!(
            "  Hello {} ms after reopening (firmware {})",
            ms(reopened),
            hello.firmware
        ),
        None => println!("  no Hello within 1 s of reopening"),
    }
    link.send(&frame::packet(
        frame::CMD_SET_DEVICE_STATE,
        frame::FLAG_SYSTEM,
        0x03,
        &[frame::STATE_START],
    ))?;
    println!("  answered with Start after {} ms", ms(start));

    let options = Options {
        response_timeout: Duration::from_secs(1),
        attempts: 2,
        ..Options::default()
    };
    let mut ignore = |_: &Event| {};
    let info = Updater::new(&mut link, options, &mut ignore).query_info()?;
    println!(
        "Controller responds again (update protocol {}), {} ms after the reset.",
        info.protocol,
        ms(start)
    );
    Ok(())
}

/// Advice for a controller that does not answer: another program may be
/// reading its replies. Names the programs holding it when they can be found.
fn no_response_hint(controller: &Controller, error: &str) -> String {
    if !error.starts_with("no response to ") {
        return String::new();
    }
    match usb::holder_hint(controller.vendor_id, controller.product_id) {
        holders if holders.is_empty() => "\nIf another program uses the controller, quit it. \
             Then unplug the controller, plug it back in and try again."
            .to_owned(),
        holders => format!("{holders}\nThen unplug the controller, plug it back in and try again."),
    }
}

fn describe(controller: &Controller) -> String {
    let (name, codename) = controller.model();
    let (major, minor) = controller.firmware_major_minor();
    format!(
        "{}  {:04x}:{:04x}  {name}{}  firmware {major}.{minor}",
        controller.location(),
        controller.vendor_id,
        controller.product_id,
        codename.map(|c| format!(" ({c})")).unwrap_or_default(),
    )
}

/// What a firmware component is, where known. Studied on the Series X|S
/// (MERL, 0b12) and One S (CRET, 02ea) controllers, which share a layout:
/// component 1 loads at 0x10000000 and is marked critical (never written by
/// the updater); component 3's version is the release version.
fn component_label(vendor_id: u16, product_id: u16, component: u32) -> Option<&'static str> {
    match (vendor_id, product_id, component) {
        (0x045e, 0x0b12 | 0x02ea, 1) => Some("bootloader, not updated"),
        (0x045e, 0x0b12 | 0x02ea, 3) => Some("application"),
        _ => None,
    }
}

fn format_component(
    vendor_id: u16,
    product_id: u16,
    version: &crate::gip::ufu::ComponentVersion,
) -> String {
    let mut text = format!("component {}: {}", version.component, version.version);
    let mut notes = Vec::new();
    if let Some(label) = component_label(vendor_id, product_id, version.component) {
        notes.push(label.to_owned());
    }
    if let Some(size) = version.partition_size {
        notes.push(format!("partition {size:#x} bytes"));
    }
    if !notes.is_empty() {
        text += &format!("  ({})", notes.join(", "));
    }
    text
}

fn list() -> Result<(), String> {
    let controllers = usb::find_controllers()?;
    if controllers.is_empty() {
        println!("No Xbox controllers found.");
    }
    for controller in &controllers {
        match controller.serial() {
            Some(serial) => println!("{}  serial {serial}", describe(controller)),
            None => println!("{}", describe(controller)),
        }
        let holders = usb::holders_of(controller);
        if !holders.is_empty() {
            println!("  {}", holders::sentence(&holders));
        }
    }
    Ok(())
}

fn info(device: Option<&str>, trace: bool) -> Result<(), String> {
    let controller = usb::select_controller(device)?;
    println!("{}", describe(&controller));
    let mut link = UsbLink::open(&controller, trace)?;
    print_info(&controller, &mut link)
}

/// Parse a decimal or `0x`-prefixed hexadecimal u32.
fn parse_u32(text: &str) -> Result<u32, String> {
    let parsed = match text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        Some(hex) => u32::from_str_radix(hex, 16),
        None => text.parse(),
    };
    parsed.map_err(|error| format!("invalid number {text:?}: {error}"))
}

/// Start the controller and print what it says about itself: the Hello and
/// the firmware components.
fn print_info(controller: &Controller, link: &mut UsbLink) -> Result<(), String> {
    if let Some(hello) = start_device(link)? {
        println!(
            "Device ID      {}",
            usb::hex(&hello.device_id).replace(' ', "")
        );
        println!("Firmware       {}", hello.firmware);
        println!("Hardware       {}", usb::hex(&hello.hardware));
    }

    // UFU queries only read; they are what the Windows updater asks first.
    let options = Options {
        response_timeout: Duration::from_secs(1),
        attempts: 2,
        ..Options::default()
    };
    let mut ignore = |_: &Event| {};
    let mut ufu = Updater::new(link, options, &mut ignore);
    let ufu_info = match ufu.query_info() {
        Ok(ufu_info) => ufu_info,
        Err(error) => {
            println!(
                "Update protocol: not available ({error}){}",
                no_response_hint(controller, &error)
            );
            return Ok(());
        }
    };
    println!(
        "Update protocol {} (flags {:#04x}), max write {:#x} bytes",
        ufu_info.protocol, ufu_info.flags, ufu_info.max_write
    );
    for component in 1..=7 {
        if let Some(version) = ufu.query_version(component)? {
            println!(
                "  {}",
                format_component(controller.vendor_id, controller.product_id, &version)
            );
        }
    }
    Ok(())
}

fn list_feed() -> Result<(), String> {
    let feed = feed::fetch_feed()?;
    println!("Feed {} version {}", feed.name, feed.version);
    for device in &feed.devices {
        let files: Vec<String> = device
            .files
            .iter()
            .map(|file| file.version().unwrap_or_else(|| file.name.clone()))
            .collect();
        let required: Vec<&str> = device.required.iter().map(|r| r.version.as_str()).collect();
        println!(
            "  {:<20} {:<10} firmware {}  (requires at least {})",
            device.id,
            device.codename,
            files.join(", "),
            required.join(", ")
        );
    }
    Ok(())
}

/// Download the feed's firmware for a controller; returns the file path and
/// the verified, parsed package.
fn download(
    device: Option<&str>,
    id: Option<&str>,
    output: Option<PathBuf>,
) -> Result<(PathBuf, Package), String> {
    let (vendor_id, product_id) = match id {
        Some(id) => id
            .split_once(':')
            .and_then(|(vid, pid)| {
                Some((
                    u16::from_str_radix(vid, 16).ok()?,
                    u16::from_str_radix(pid, 16).ok()?,
                ))
            })
            .ok_or_else(|| format!("invalid --id {id:?}; use VID:PID, e.g. 045e:0b12"))?,
        None => {
            let controller = usb::select_controller(device)?;
            (controller.vendor_id, controller.product_id)
        }
    };
    let feed = feed::fetch_feed()?;
    let entry = feed.device(vendor_id, product_id).ok_or_else(|| {
        format!("Microsoft's firmware feed has no entry for {vendor_id:04x}:{product_id:04x}")
    })?;
    let file = entry
        .files
        .first()
        .ok_or_else(|| format!("the feed lists no firmware for {}", entry.id))?;
    let directory = output.unwrap_or_else(feed::cache_dir);
    let (path, package) = feed::download(&feed, file, &directory)?;
    println!(
        "{} ({}) firmware {} [{}], SHA-256 verified",
        entry.id, entry.codename, package.version, file.tag
    );
    println!("{}", path.display());
    Ok((path, package))
}

/// Install `image`, or the feed's current firmware when `image` is `None`.
/// Options for `update` beyond the upstream ones: component selection,
/// forced rewrites and diagnostic hashes.
struct UpdateExtras {
    only: Vec<u32>,
    force: bool,
    probe: Vec<(u32, u32)>,
    /// Flash range to back up during the update, and the file to save it to.
    backup: Option<(u32, u32, PathBuf)>,
}

/// Parse `ADDRESS:LENGTH`, each decimal or 0x-hexadecimal.
fn parse_range(text: &str) -> Result<(u32, u32), String> {
    let (address, length) = text
        .split_once(':')
        .ok_or_else(|| format!("expected ADDRESS:LENGTH, got {text:?}"))?;
    let length = parse_u32(length)?;
    if length == 0 {
        return Err("length must be at least 1".to_owned());
    }
    Ok((parse_u32(address)?, length))
}

fn update(
    image: Option<&Path>,
    device: Option<&str>,
    dry_run: bool,
    yes: bool,
    extra: UpdateExtras,
    trace: bool,
) -> Result<(), String> {
    // Check the backup file before a long run, not after it.
    if let Some((_, length, path)) = &extra.backup {
        if *length == 0 {
            return Err("backup length must be at least 1".to_owned());
        }
        if path.exists() {
            return Err(format!(
                "{} already exists; choose another file so no backup is overwritten",
                path.display()
            ));
        }
    }
    let mut package = match image {
        Some(image) => {
            let bytes = fs::read(image)
                .map_err(|error| format!("could not read {}: {error}", image.display()))?;
            Package::parse(&bytes)?
        }
        None => download(device, None, None)?.1,
    };
    if !extra.only.is_empty() {
        if let Some(missing) = extra
            .only
            .iter()
            .find(|id| !package.components.iter().any(|c| c.id == **id))
        {
            return Err(format!("the package has no component {missing}"));
        }
        // Components 4 and 3 target the same flash: writing 4 replaces the
        // application, which only a following write of 3 restores.
        if extra.only.contains(&4) && !extra.only.contains(&3) {
            return Err(
                "component 4 shares flash with component 3 (the application); \
                 use --only 4,3 so the application is written back afterwards"
                    .to_owned(),
            );
        }
        package.components.retain(|c| extra.only.contains(&c.id));
        println!("Only updating component(s) {:?}", extra.only);
    }
    println!(
        "Package: firmware {} for {:04x}:{:04x}",
        package.version, package.vendor_id, package.product_id
    );
    if dry_run {
        for component in &package.components {
            println!(
                "  component {}: {:<12} {:#x} bytes at {:#010x}{}",
                component.id,
                component.version.to_string(),
                component.data.len(),
                component.target_address,
                if component.is_critical() {
                    "  (critical: not written)"
                } else {
                    ""
                }
            );
        }
    }

    let controller = match usb::select_controller(device) {
        Ok(controller) => controller,
        Err(error) if dry_run => {
            println!("Not comparing with a controller: {error}");
            return Ok(());
        }
        Err(error) => return Err(error),
    };
    if (controller.vendor_id, controller.product_id) != (package.vendor_id, package.product_id) {
        return Err(format!(
            "the package is for {:04x}:{:04x}, but {} is {:04x}:{:04x}",
            package.vendor_id,
            package.product_id,
            controller.location(),
            controller.vendor_id,
            controller.product_id
        ));
    }
    println!("Controller: {}", describe(&controller));

    // A program that has the controller open makes the update flaky (it can
    // take the controller's replies), so say so before touching it. The user
    // decides at the confirmation prompt.
    let holders = usb::holders_of(&controller);
    let holder_warning = (!holders.is_empty()).then(|| {
        let quit = if holders.len() == 1 { "it" } else { "them" };
        format!(
            "Warning: {}. Quit {quit}, unplug the controller, plug it back in and run \
             this command again, or the update may fail.",
            holders::sentence(&holders)
        )
    });
    if let Some(warning) = &holder_warning {
        println!("{warning}");
    }
    let mut link = UsbLink::open(&controller, trace)?;
    start_device(&mut link)?;

    // Read-only: compare the controller with the package.
    let mut ignore = |_: &Event| {};
    let plan = Updater::new(&mut link, Options::default(), &mut ignore)
        .plan(&package)
        .map_err(|error| format!("{error}{}", no_response_hint(&controller, &error)))?;
    for planned in &plan.components {
        if planned.needs_update() {
            println!(
                "  component {}: {} -> {}",
                planned.id, planned.device, planned.package
            );
        } else {
            println!(
                "  component {}: {} (up to date)",
                planned.id, planned.device
            );
        }
    }
    // Force only a device that is complete. If a component already differs,
    // an earlier run stopped partway: finish it normally. Forcing again would
    // rewrite 4 (overwriting the application) before 3 is ever restored.
    let force = extra.force && !plan.needs_update();
    if force {
        println!("  --force: rewriting the selected components anyway");
    } else if extra.force {
        println!("  --force ignored: an earlier update is unfinished; completing it first");
    }
    if !plan.needs_update() && !force {
        println!("The controller already runs this firmware; nothing to do.");
        return Ok(());
    }
    if dry_run {
        return Ok(());
    }
    if !yes {
        if let Some(warning) = &holder_warning {
            println!("\n{warning}");
        }
        println!(
            "\nThis writes firmware to the controller and restarts it. Keep it connected \
             until the update finishes; an interrupted update can leave it unusable."
        );
        print!("Type 'update' to continue: ");
        io::stdout().flush().ok();
        let mut answer = String::new();
        io::stdin()
            .lock()
            .read_line(&mut answer)
            .map_err(|error| error.to_string())?;
        if answer.trim() != "update" {
            return Err("cancelled".to_owned());
        }
    }

    let mut progress = Progress::new();
    let mut report = |event: &Event| progress.event(event);
    // Already written blocks are skipped on the next run, so an interrupted
    // update continues where it stopped.
    let options = Options {
        force,
        probe: extra.probe,
        backup: extra.backup.as_ref().map(|(address, length, _)| (*address, *length)),
        ..Options::default()
    };
    let result = Updater::new(&mut link, options, &mut report).run(&package);
    // Save the backup even if the update failed afterwards: it was verified
    // block by block before anything was written.
    if let Some((_, _, path)) = &extra.backup {
        match progress.backup.take() {
            Some((address, data)) => {
                use sha2::{Digest, Sha256};
                fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(path)
                    .and_then(|mut file| file.write_all(&data))
                    .map_err(|error| format!("could not write {}: {error}", path.display()))?;
                println!(
                    "Backup saved: {} ({:#x} bytes from {address:#010x}, SHA-256 {})",
                    path.display(),
                    data.len(),
                    usb::hex(&Sha256::digest(&data)).replace(' ', "")
                );
            }
            None => println!("No backup was made."),
        }
    }
    result.map_err(|error| {
            format!(
                "{error}{}\nUnplug the controller, plug it back in and run the same command \
                 again: the update continues with the components that still differ.",
                usb::holder_hint(controller.vendor_id, controller.product_id)
            )
        })?;

    // The final reset restarts the controller on USB. Reopen it at its new
    // address and answer its Hello; a restarted controller that nobody
    // answers stays unresponsive until it is unplugged.
    println!("Waiting for the controller to restart...");
    // Windows allows 60 s for the slowest restart (after component 7); give
    // more so a controller that needs a replug on macOS can still be resumed.
    let restarted = link.reconnect(Duration::from_secs(120)).and_then(|()| {
        start_device(&mut link)?;
        let options = Options {
            response_timeout: Duration::from_secs(1),
            attempts: 2,
            ..Options::default()
        };
        let mut ignore = |_: &Event| {};
        let mut ufu = Updater::new(&mut link, options, &mut ignore);
        let mut versions = Vec::new();
        for component in package.components.iter().filter(|c| !c.is_critical()) {
            if let Some(version) = ufu.query_version(component.id)? {
                versions.push(format_component(
                    package.vendor_id,
                    package.product_id,
                    &version,
                ));
            }
        }
        Ok(versions)
    });
    match restarted {
        Ok(versions) => {
            println!("Done. The controller now runs:");
            for version in versions {
                println!("  {version}");
            }
        }
        Err(error) => println!(
            "Update done, but the restarted controller did not answer ({error}). \
             If it stays unresponsive, unplug it and plug it back in."
        ),
    }
    Ok(())
}

/// Prints update progress: one line per component, rewritten in place on a
/// terminal.
struct Progress {
    terminal: bool,
    /// Component being written.
    current: Option<(u32, Version)>,
    /// Width of the progress line currently on screen.
    open_line: Option<usize>,
    last_step: Option<u64>,
    /// Verified backup data (address, bytes), kept for the caller to save.
    backup: Option<(u32, Vec<u8>)>,
}

impl Progress {
    fn new() -> Self {
        Self {
            terminal: io::stdout().is_terminal(),
            current: None,
            open_line: None,
            last_step: None,
            backup: None,
        }
    }

    /// Show `text` as the current progress line: rewritten in place on a
    /// terminal, otherwise printed at every quarter.
    fn progress_line(&mut self, text: String, percent: u64) {
        if self.terminal {
            let width = self.open_line.unwrap_or(0);
            print!("\r{text:<width$}");
            io::stdout().flush().ok();
            self.open_line = Some(text.len());
        } else if self.last_step != Some(percent / 25) {
            self.last_step = Some(percent / 25);
            println!("{text}");
        }
    }

    /// Keep the progress line and move on, before printing anything else.
    fn break_line(&mut self) {
        if self.open_line.take().is_some() {
            println!();
        }
    }

    fn event(&mut self, event: &Event) {
        match event {
            Event::Begin {
                component, version, ..
            } => {
                self.break_line();
                self.current = Some((*component, *version));
                self.last_step = None;
            }
            Event::Block { done, total, .. } => {
                let Some((component, version)) = self.current else {
                    return;
                };
                let percent = u64::from(*done) * 100 / u64::from(*total);
                let text = format!("  component {component}: writing {version} {percent:>3}%");
                if self.terminal {
                    let width = self.open_line.unwrap_or(0);
                    print!("\r{text:<width$}");
                    io::stdout().flush().ok();
                    self.open_line = Some(text.len());
                } else if self.last_step != Some(percent / 25) {
                    self.last_step = Some(percent / 25);
                    println!("{text}");
                }
            }
            Event::Complete { component } => {
                if let Some(width) = self.open_line.take() {
                    print!("\r{:width$}\r", "");
                }
                match self.current.take() {
                    Some((_, version)) => println!("  component {component}: {version} written"),
                    None => println!("  component {component}: written"),
                }
            }
            Event::Reset => {
                self.break_line();
                println!("  restarting the controller...");
            }
            Event::Probe {
                address,
                length,
                result,
            } => {
                use sha1::{Digest, Sha1};
                self.break_line();
                match result {
                    Ok(digest) => {
                        let note = if Sha1::digest(vec![0xff; *length as usize])[..] == digest[..] {
                            "  (erased: all 0xff)"
                        } else if Sha1::digest(vec![0x00; *length as usize])[..] == digest[..] {
                            "  (all 0x00)"
                        } else {
                            ""
                        };
                        println!(
                            "  probe {address:#010x} +{length:#x}  {}{note}",
                            usb::hex(digest).replace(' ', "")
                        );
                    }
                    Err(error) => println!("  probe {address:#010x} +{length:#x}  failed: {error}"),
                }
            }
            Event::BackupProgress { done, total } => {
                let percent = u64::from(*done) * 100 / u64::from(*total);
                self.progress_line(
                    format!("  backup: {done:#x} of {total:#x} bytes verified {percent:>3}%"),
                    percent,
                );
            }
            Event::BackupDone { address, data } => {
                if let Some(width) = self.open_line.take() {
                    print!("\r{:width$}\r", "");
                }
                self.last_step = None;
                println!(
                    "  backup: {:#x} bytes at {address:#010x} read, every 4 KiB block verified",
                    data.len()
                );
                self.backup = Some((*address, data.clone()));
            }
            Event::BackupFailed(error) => {
                self.break_line();
                self.last_step = None;
                println!("  backup FAILED: {error} (the update continues)");
            }
            Event::Reconnected => println!("  controller is back, continuing"),
            Event::Verified => {
                self.break_line();
                println!("All components verified.");
            }
            Event::Skipped { component, reason } => {
                self.break_line();
                println!("  component {component}: skipped ({reason})");
            }
            Event::Retry { op, attempt } => {
                self.break_line();
                eprintln!(
                    "  no response to {}, retrying (attempt {attempt})",
                    crate::gip::ufu::op_name(*op)
                );
            }
            Event::FragmentResend { offset } => {
                self.break_line();
                eprintln!("  transfer interrupted; resending from byte {offset}");
            }
            Event::Info(_)
            | Event::DeviceVersion { .. }
            | Event::Order(_)
            | Event::UpToDate { .. } => {}
        }
    }
}

/// Send Power On and briefly watch for a Hello. The controller announces
/// itself only once after it starts, and on Linux `xpad` usually claims the
/// device and receives that Hello first, so it is often missing.
fn start_device(link: &mut UsbLink) -> Result<Option<Hello>, String> {
    // Set Device State: Start. A non-zero sequence number, as in the update
    // path that answers a restarted controller's Hello.
    link.send(&[0x05, 0x20, 0x01, 0x01, 0x00])?;
    let deadline = Instant::now() + Duration::from_millis(500);
    while let Some(bytes) = link.recv(deadline.saturating_duration_since(Instant::now()))? {
        if let Some(hello) = Packet::parse(&bytes).as_ref().and_then(Hello::parse) {
            return Ok(Some(hello));
        }
    }
    Ok(None)
}
