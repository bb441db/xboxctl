---
title: xboxctl
nav_order: 1
description: xboxctl is an open source command line tool that updates Xbox controller firmware on Linux and macOS, with no Windows PC or Xbox Accessories app needed.
---

# Update Xbox controller firmware on Linux and macOS
{: .no_toc }

`xboxctl` inspects and updates the firmware of Xbox (GIP) controllers from
Linux and macOS, without the Xbox Accessories app. It speaks the same
[firmware update protocol]({% link protocol.md %}) ("UFU", carried in GIP command `0x0c`) as the
Windows updater and downloads firmware from the same Microsoft feed.

<details open markdown="block">
  <summary>Contents</summary>
  {: .text-delta }
- TOC
{:toc}
</details>

## Supported controllers

Controllers must be connected with a USB cable; updates over the Xbox
Wireless Adapter or Bluetooth are not supported.

| Controller | USB ID | Codename | Tested |
|------------|--------|----------|:------:|
| Xbox Series X\|S Controller | `045e:0b12` | MERL | ✅ |
| Xbox One S Controller | `045e:02ea` | CRET | ✅ |
| Xbox Elite Controller Series 2 | `045e:0b00` | DELP | ❌ |
| Xbox Adaptive Controller | `045e:0b0a` | ZEPH | ❌ |
| Xbox Adaptive Joystick | `045e:0b1a`, `045e:0b1c` | DURA | ❌ |
| Xbox One Elite Controller | `045e:02e3` | TROY | ❌ |
| Xbox One Controller (2015) | `045e:02dd` | LANG | ❌ |
| Xbox One Controller (original) | `045e:02d1`, `045e:02d2` | ARGO, ARGD | ❌ |

The original Xbox One controller uses a legacy update protocol that `xboxctl`
does not support.

Untested controllers may well work: `xboxctl info` and `xboxctl update --latest
--dry-run` are read-only and show whether a controller speaks the update
protocol and what an update would change. If you try one, please
[open an issue](https://github.com/bb441db/xboxctl/issues) with the result.

Microsoft's feed also lists models that `xboxctl` has never seen (codenames
NORL, ORRE, PARK, NICO); their firmware layouts suggest accessories other than
controllers. Feed entries with a `-cc-` suffix (variants of `0b17` and `0b32`)
cannot be selected yet, and the SCUF controller (`2e95:0504`) gets MERL
firmware, which `update` refuses because the USB IDs differ.

## Installation

On Linux (x86_64, aarch64) and Apple Silicon Macs:

```bash
curl -fsSL https://raw.githubusercontent.com/bb441db/xboxctl/main/install.sh | sh
```

Or with `wget`:

```bash
wget -qO- https://raw.githubusercontent.com/bb441db/xboxctl/main/install.sh | sh
```

The script downloads the binary for your system from the
[latest release](https://github.com/bb441db/xboxctl/releases/latest), checks
its SHA-256 and installs it to `~/.local/bin` (Linux) or `/usr/local/bin`
(macOS). Set `XBOXCTL_INSTALL_DIR` to install elsewhere, or `XBOXCTL_VERSION`
to a release tag to install that release. The binaries have no runtime
dependencies; you can also download them from the releases page directly.

On Linux, install the udev rule below so that `xboxctl` runs without root. On
macOS, run it with `sudo`; see [Permissions](#permissions).

## Usage

```bash
xboxctl list                       # connected controllers (read-only)
xboxctl info                       # identity and per-component firmware versions (read-only)
xboxctl download                   # current firmware for the connected model
xboxctl download --list            # every model in Microsoft's feed
xboxctl update --latest --dry-run  # compare the controller with the current firmware
xboxctl update --latest            # install the current firmware
xboxctl update <package.bin>       # install an rdhm package file
```

With several controllers connected, choose one with `--device BUS:ADDRESS`
from `xboxctl list`.

`update` first shows what would change and stops if the controller is already
up to date. During an update the controller restarts several times (it
re-enumerates on USB) and `xboxctl` reconnects to it. Keep it connected until
the update finishes.

Downloads are checked against the SHA-256 in their file name and cached in the
user cache directory (`~/.cache/xboxctl/firmware` on Linux,
`~/Library/Caches/xboxctl/firmware` on macOS). Under `sudo` the files still go
to, and belong to, the user who ran it.

## Permissions

### Linux

`xboxctl` detaches `xpad` from the controller while it runs and needs
read/write access to the controller's USB device node. Install this udev rule
once to run it without root. It gives the logged-in user access to GIP
controllers (USB class `ff/47/d0`):

```bash
echo 'SUBSYSTEM=="usb", ENV{DEVTYPE}=="usb_device", ATTR{bDeviceClass}=="ff", ATTR{bDeviceSubClass}=="47", ATTR{bDeviceProtocol}=="d0", TAG+="uaccess"' \
  | sudo tee /etc/udev/rules.d/70-xboxctl.rules
sudo udevadm control --reload && sudo udevadm trigger
```

### macOS

A system driver (`XboxUSBDevice`) holds the controller exclusively, and there
is no udev equivalent for taking it over. Commands that talk to the controller
(`info`, `update`, ...) must run as root:

```bash
sudo xboxctl info
sudo xboxctl update --latest
```

`list` and `download` do not need root.

## Building from source

Build with Rust (the toolchain is pinned in `rust-toolchain.toml`) and a C
compiler; libusb is compiled in.

```bash
git clone https://github.com/bb441db/xboxctl
cd xboxctl
cargo build --release      # target/release/xboxctl
```

## License

[0BSD](https://github.com/bb441db/xboxctl/blob/main/LICENSE).
