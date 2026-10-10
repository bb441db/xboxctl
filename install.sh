#!/bin/sh
# Install xboxctl from the GitHub releases.
#
#   curl -fsSL https://raw.githubusercontent.com/bb441db/xboxctl/main/install.sh | sh
#
# Environment:
#   XBOXCTL_INSTALL_DIR  where to install (default: ~/.local/bin on Linux,
#                        /usr/local/bin on macOS)
#   XBOXCTL_VERSION      release tag to install (default: the latest release)
set -eu

repo="bb441db/xboxctl"
udev_rule='SUBSYSTEM=="usb", ENV{DEVTYPE}=="usb_device", ATTR{bDeviceClass}=="ff", ATTR{bDeviceSubClass}=="47", ATTR{bDeviceProtocol}=="d0", TAG+="uaccess"'

say() { printf '%s\n' "$*"; }
fail() { printf 'xboxctl install: %s\n' "$*" >&2; exit 1; }

case "$(uname -s)" in
    Linux) os=linux ;;
    Darwin) os=macos ;;
    *) fail "unsupported system $(uname -s); xboxctl supports Linux and macOS" ;;
esac
case "$(uname -m)" in
    x86_64 | amd64) arch=x86_64 ;;
    aarch64 | arm64) arch=aarch64 ;;
    *) fail "unsupported architecture $(uname -m)" ;;
esac
if [ "$os" = macos ] && [ "$arch" != aarch64 ]; then
    fail "only Apple Silicon Macs are supported"
fi

asset="xboxctl-$os-$arch"
if [ -n "${XBOXCTL_VERSION:-}" ]; then
    base="https://github.com/$repo/releases/download/$XBOXCTL_VERSION"
else
    base="https://github.com/$repo/releases/latest/download"
fi

if [ -n "${XBOXCTL_INSTALL_DIR:-}" ]; then
    install_dir="$XBOXCTL_INSTALL_DIR"
elif [ "$os" = macos ]; then
    # xboxctl runs under sudo on macOS, and sudo does not search ~/.local/bin.
    install_dir=/usr/local/bin
else
    install_dir="$HOME/.local/bin"
fi

if command -v curl >/dev/null 2>&1; then
    fetch() { curl -fsSL -o "$2" "$1"; }
elif command -v wget >/dev/null 2>&1; then
    fetch() { wget -q -O "$2" "$1"; }
else
    fail "curl or wget is required"
fi

if command -v sha256sum >/dev/null 2>&1; then
    sha256() { sha256sum "$1" | cut -d ' ' -f 1; }
elif command -v shasum >/dev/null 2>&1; then
    sha256() { shasum -a 256 "$1" | cut -d ' ' -f 1; }
else
    fail "sha256sum or shasum is required"
fi

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

say "Downloading $asset ${XBOXCTL_VERSION:-(latest release)}"
fetch "$base/$asset" "$tmp/$asset" || fail "could not download $base/$asset"
fetch "$base/$asset.sha256" "$tmp/$asset.sha256" ||
    fail "could not download $base/$asset.sha256"

expected=$(cut -d ' ' -f 1 "$tmp/$asset.sha256")
actual=$(sha256 "$tmp/$asset")
[ -n "$expected" ] && [ "$expected" = "$actual" ] ||
    fail "checksum mismatch for $asset (expected $expected, got $actual)"
chmod 755 "$tmp/$asset"

if mkdir -p "$install_dir" 2>/dev/null && [ -w "$install_dir" ]; then
    mv "$tmp/$asset" "$install_dir/xboxctl"
else
    say "Installing to $install_dir requires sudo"
    sudo mkdir -p "$install_dir"
    sudo mv "$tmp/$asset" "$install_dir/xboxctl"
fi
say "Installed $("$install_dir/xboxctl" --version) to $install_dir/xboxctl"

case ":$PATH:" in
    *":$install_dir:"*) ;;
    *) say "Note: $install_dir is not in your PATH; add it to run xboxctl directly." ;;
esac

if [ "$os" = linux ]; then
    if [ ! -e /etc/udev/rules.d/70-xboxctl.rules ]; then
        say ""
        say "To use xboxctl without root, install the udev rule once:"
        say ""
        say "  echo '$udev_rule' \\"
        say "    | sudo tee /etc/udev/rules.d/70-xboxctl.rules"
        say "  sudo udevadm control --reload && sudo udevadm trigger"
    fi
else
    say ""
    say "On macOS, run commands that talk to the controller with sudo, e.g. sudo xboxctl info"
fi
