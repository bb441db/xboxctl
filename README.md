# xboxctl

Inspect and update firmware on supported Xbox controllers from Linux and
macOS, without the Xbox Accessories app. GIP controllers use Microsoft's UFU
protocol; the Xbox Nano uses Microsoft's Companion Protocol over HID. Firmware
comes from Microsoft's public feed for GIP devices and its flight feed for the
Nano.

## Supported controllers

Controllers must be connected with a USB cable; updates over the Xbox
Wireless Adapter or Bluetooth are not supported.

| Controller | USB ID | Codename | Tested |
|------------|--------|----------|:------:|
| Xbox Series X\|S Controller | `045e:0b12` | MERL | ✅ |
| Xbox Nano Controller | `045e:0b4e` | TALA | ✅ |
| Xbox One S Controller | `045e:02ea` | CRET | ✅ |
| Xbox Elite Controller Series 2 | `045e:0b00` | DELP | ❌ |
| Xbox Adaptive Controller | `045e:0b0a` | ZEPH | ❌ |
| Xbox Adaptive Joystick | `045e:0b1a`, `045e:0b1c` | DURA | ❌ |
| Xbox One Elite Controller | `045e:02e3` | TROY | ❌ |
| Xbox One Controller (2015) | `045e:02dd` | LANG | ❌ |
| Xbox One Controller (original) | `045e:02d1`, `045e:02d2` | ARGO, ARGD | ❌ |

The original Xbox One controller uses a legacy update protocol that `xboxctl`
does not support. Untested models may work; `xboxctl info` and
`xboxctl update --latest --dry-run` can check support without writing firmware.

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

On Linux, install the udev rule so that `xboxctl` runs without root. On
macOS, run it with `sudo`. See the documentation for [permissions](https://bb441db.github.io/xboxctl/#permissions).

## Documentation

- [Usage, permissions, and calibration](https://bb441db.github.io/xboxctl/)
- [Xbox Nano Controller](https://bb441db.github.io/xboxctl/nano/)
- [Firmware update protocol](https://bb441db.github.io/xboxctl/protocol/)
- [SteamOS guide](https://bb441db.github.io/xboxctl/steamos/)
- [macOS wired controller driver](https://bb441db.github.io/xboxctl/macos-driver/)
- [Building from source](https://bb441db.github.io/xboxctl/#building-from-source)
