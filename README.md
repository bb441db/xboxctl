# Xbox USB controller research

Rust workspace for reverse engineering an Xbox controller connected over USB.

## Workspace

- `gip` (`crates/gip`): GIP framing and the UFU firmware update protocol, both
  the host side (updater) and the device side (simulated flash). The protocol is
  documented in [`docs/firmware-update-host.md`](docs/firmware-update-host.md).
- `xbox-virtual-device` (`crates/xbox-virtual-device`): a virtual controller on
  Linux FunctionFS, including firmware updates.
- `xboxctl`: command-line firmware inspection and firmware updates.

## Firmware updates

`xboxctl update <package>` installs an rdhm firmware package: the
`MERL_045E_0B12_v_*.bin` files that ship with the Xbox Accessories app. It runs
the same UFU sequence as the Windows updater. The protocol has been verified
against emulated controllers only, so use `--dry-run` to inspect a package and
test against the QEMU `usb-xbox-gip` device or the Rust virtual controller
before using it on hardware.

```bash
cargo run -p xboxctl -- update --dry-run MERL_045E_0B12_v_5_23_6_0_*.bin
cargo run -p xboxctl -- update MERL_045E_0B12_v_5_23_6_0_*.bin
```

On Linux the tool detaches `xpad` from the controller and needs read/write
access to its USB device node. `xboxctl` uses libusb through `rusb`, so it also
builds on Windows and macOS: build with `--features vendored-libusb` to compile
libusb from source. On Windows the controller must be bound to WinUSB (for
example with Zadig) instead of `xboxgip.sys`.

To make a virtual controller offer an update, start it with an older version:
`sudo XBOX_FIRMWARE=5.17.3202.0 scripts/run-virtual-controller.sh`, or add
`fw-major=5,fw-minor=17,fw-build=3202,fw-revision=0` to the QEMU device.

`xbox-virtual-device` now has a Linux FunctionFS transport for its GIP protocol
core. This host has the USB/IP virtual UDC module, so the gadget can be exported
over USB/IP and attached from the Windows VM using a Windows USB/IP client.
VirtualBox's USB device list only shows host USB devices; it will not list this
software-created gadget directly.

To build and start the virtual controller on this Linux host:

```bash
cargo build -p xbox-virtual-device
sudo scripts/run-virtual-controller.sh
```

The process logs each GIP packet and state transition. The gadget uses the
captured `045e:0b12` identity and the physical controller's captured GIP device
ID (`7eed8da38daa0000`) and sends Hello every 500 ms until the host requests
metadata or starts the device. The Windows VM
must be able to reach the Linux host over TCP port 3240. Install a Windows
USB/IP client such as [usbip-win2](https://github.com/vadimgrn/usbip-win2) in
the VM, then use an elevated PowerShell prompt while the Linux script is
running:

```powershell
usbip.exe list -r <linux-host-ip>
usbip.exe attach -r <linux-host-ip> -b usbip-vudc.0
```

The USB/IP client creates the virtual USB connection inside Windows; it will
not appear in VirtualBox's host USB device list. The FunctionFS backend exposes
the captured interface and alternate-setting descriptors. FunctionFS requires
different endpoint addresses for interface 0's alternate setting 1, so that
setting uses `0x05`/`0x85` in the gadget. The emulator does not yet implement
secondary audio/bulk endpoint traffic or advertise Microsoft OS descriptors
that have not been verified from a physical-device capture.

`xboxctl firmware` starts an interrupt-IN read, then sends the observed GIP
Power On, LED, and Authenticate initialization messages in sequence. It reports
the firmware version if it receives Hello Device. These are startup commands,
not firmware writes. Increase the listen window with `--timeout-seconds 60` if
needed; use `--trace-packets` to show interrupt traffic in both directions.

`xboxctl firmware --capture <file.pcapng>` reads that Hello message from a
USBPcap capture. The startup capture reports `5.23.6.0`. This is device version
information, not a firmware image dump. The metadata exchange in the capture
contains descriptive GIP data; it does not contain firmware bytes.

Live inspection requires the controller to be available to Linux and its
interface to be free for `xboxctl` to claim. If Linux `xpad` has claimed it,
unbind that interface before running the command. The captured controller is
USB VID:PID `045e:0b12`; firmware writes remain disabled until the update
protocol and image format are understood.

## Build and command help

```powershell
cargo build --workspace
cargo run -p xboxctl -- --help
cargo run -p xboxctl -- firmware --timeout-seconds 60
cargo run -p xboxctl -- firmware --capture captures/xbox-full-startup.pcapng
cargo run -p xboxctl -- firmware --capture captures/vibrate-command.pcapng
cargo run -p xbox-virtual-device
```

## QEMU experiments

The QEMU-based Windows VM workflow, including the custom Xbox GIP device build
and physical-controller USB capture command, is documented in
[`docs/qemu-workflow.md`](docs/qemu-workflow.md).

## Research sequence

1. Decode GIP Hello and metadata exchanges to identify the device and firmware.
2. Capture the official firmware update flow and document the image format,
   message framing, transfer direction, lengths, and responses.
3. Implement firmware updates only after the update protocol is understood and
   recovery behavior is documented.

Keep captures and notes in `docs/` and distinguish observed facts from guesses.
Avoid issuing undocumented write or firmware-update transfers to the physical
controller.
