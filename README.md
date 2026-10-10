# xboxctl

Inspect and update the firmware of Xbox (GIP) controllers from Linux, without
the Xbox Accessories app. It speaks the same firmware update protocol ("UFU",
carried in GIP command `0x0c`) as the Windows updater and downloads firmware
from the same Microsoft feed.

Tested on an Xbox Series X|S controller (`045e:0b12`, codename MERL) and
against the QEMU-emulated controller in `submodules/qemu`.

## Usage

```bash
cargo run -- list                       # connected controllers (read-only)
cargo run -- info                       # identity and per-component firmware versions (read-only)
cargo run -- download                   # current firmware for the connected model
cargo run -- download --list            # every model in Microsoft's feed
cargo run -- update --latest --dry-run  # compare the controller with the current firmware
cargo run -- update --latest            # install the current firmware
cargo run -- update <package.bin>       # install an rdhm package file
```

With several controllers connected, choose one with `--device BUS:ADDRESS`
from `xboxctl list`.

`update` first shows what would change and stops if the controller is already
up to date. During an update the controller restarts (it re-enumerates on USB)
and `xboxctl` reconnects to it. Keep it connected until the update finishes.

Downloads are checked against the SHA-256 in their file name and cached in the
user cache directory (`~/.cache/xboxctl/firmware` on Linux).

## Permissions

On Linux, `xboxctl` detaches `xpad` from the controller while it runs and needs
read/write access to the controller's USB device node. Install this udev rule
once to run it without root. It gives the logged-in user access to GIP
controllers (USB class `ff/47/d0`):

```bash
echo 'SUBSYSTEM=="usb", ENV{DEVTYPE}=="usb_device", ATTR{bDeviceClass}=="ff", ATTR{bDeviceSubClass}=="47", ATTR{bDeviceProtocol}=="d0", TAG+="uaccess"' \
  | sudo tee /etc/udev/rules.d/70-xboxctl.rules
sudo udevadm control --reload && sudo udevadm trigger
```

## Development

- `src/gip/`: GIP framing, the rdhm package format and the UFU updater. Its
  tests run updates against a simulated controller (`src/gip/device.rs`).
- `src/usb.rs`: controller discovery and the libusb transport.
- `src/feed.rs`: Microsoft's firmware feed.
- `submodules/qemu`: QEMU with the `usb-xbox-gip` device, an emulated
  controller for testing updates in a VM (Windows or Linux guest). Start it with
  an older version, e.g. `fw-major=5,fw-minor=17,fw-build=3202,fw-revision=0`,
  to make an update available.

```bash
cargo test
GIP_TEST_PACKAGE=/path/to/MERL_*.bin cargo test -- --ignored   # full update with an official package
```
