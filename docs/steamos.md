---
title: Steam Deck and Steam Machine
nav_order: 2
description: "Update Xbox controller firmware on a Steam Deck, Steam Machine or any SteamOS device with xboxctl: a step by step guide, no Windows PC needed."
---

# Update Xbox controller firmware on Steam Deck and Steam Machine (SteamOS)
{: .no_toc }

This guide shows how to check and update the firmware of an Xbox controller on
a Steam Deck, a Steam Machine or any other device running SteamOS, without a
Windows PC or the Xbox Accessories app.

<details open markdown="block">
  <summary>Contents</summary>
  {: .text-delta }
- TOC
{:toc}
</details>

## What you need

- A Steam Deck, Steam Machine or other SteamOS device, connected to the
  internet.
- An Xbox controller and a **USB cable** that carries data (some cables only
  charge). Updates do not work over Bluetooth or the Xbox Wireless Adapter. See
  the [list of supported controllers]({% link index.md %}#supported-controllers).
- A keyboard is very helpful. On a Steam Deck you can use the touchscreen and
  the on-screen keyboard (press the **Steam** button + **X**).

`xboxctl` only changes the controller when you run `xboxctl update`. The
commands `list`, `info` and `update --dry-run` only read from it.

## 1. Switch to desktop mode

If you are reading this on your device, you are probably already in desktop
mode. If not:

1. Press the **Steam** button and open the **Power** menu.
2. Choose **Exit to Desktop**.

## 2. Open a terminal

Open the application launcher and search for **Konsole**. To paste
into the terminal, use **Ctrl + Shift + V**.

## 3. Install xboxctl

Paste this command and press **Enter**:

```bash
curl -fsSL https://raw.githubusercontent.com/bb441db/xboxctl/main/install.sh | sh
```

The script downloads the program for your device, checks
that the download is intact and puts it in `~/.local/bin`.

When it finishes it prints a **Note** that `~/.local/bin` is not in your
`PATH`. The `PATH` is the list of places where the terminal looks for programs,
and SteamOS does not include this folder by default. Copy the command the
script printed (it looks like the first one below), run it, then run the second
command to load the change into the terminal you have open:

```bash
echo 'export PATH="$HOME/.local/bin:$PATH"' >> ~/.bashrc
source ~/.bashrc
```

Verify the program installed correctly:

```bash
command -v xboxctl
xboxctl --version
```

The first command should print a path ending in `.local/bin/xboxctl`, the
second a version number. If either prints nothing or an error, see
[Troubleshooting](#troubleshooting).

## 4. Allow xboxctl to use the controller

Normal users on Linux are not allowed to talk to USB devices directly. You fix
that once by adding a rule (a *udev rule*) that gives the logged-in user access
to Xbox controllers. This is the only step that needs administrator rights.

### Set a password (first time only)

On a fresh SteamOS install no administrator password is set yet. If you have
already set one, skip this. Otherwise set one first:

```bash
passwd
```

### Add the rule

Run these two commands and enter your password when asked:

```bash
echo 'SUBSYSTEM=="usb", ENV{DEVTYPE}=="usb_device", ATTR{bDeviceClass}=="ff", ATTR{bDeviceSubClass}=="47", ATTR{bDeviceProtocol}=="d0", TAG+="uaccess"' \
  | sudo tee /etc/udev/rules.d/70-xboxctl.rules
sudo udevadm control --reload && sudo udevadm trigger
```

The rule only matches Xbox (GIP) controllers. You only do this once.

## 5. Connect the controller

Plug the controller into the device with the USB cable. If it is already
plugged in, unplug it and plug it back in, just to be sure the new rule
applies.

## 6. Check that it is detected

```bash
xboxctl list
```

This shows your controller. Then read its details:

```bash
xboxctl info
```

This prints the controller model and the firmware version of each of its
components. If both commands work, everything is set up correctly.

If `xboxctl list` shows nothing, or `info` says it has no permission, see
[Troubleshooting](#troubleshooting).

## 7. Update the firmware

See what an update would change without changing anything:

```bash
xboxctl update --latest --dry-run
```

> **Do not unplug the controller during the update.** It restarts several
> times and may disappear and come back on its own; that is expected. Keep the
> cable connected and keep the device powered (plug in the charger on a Steam
> Deck) until the command finishes. If an update stops partway, unplug the
> controller, plug it back in and run the same command again: it continues
> where it left off.

Then install the newest firmware:

```bash
xboxctl update --latest
```

If the controller already has the newest firmware, `xboxctl` says so and stops.

## Troubleshooting

### `xboxctl: command not found`

`~/.local/bin` is not in your `PATH`. Run the `echo ... >> ~/.bashrc` command
from [step 3](#3-install-xboxctl), then run `source ~/.bashrc` or open a new
terminal.

### `xboxctl list` shows no controller

Check that the cable is a data cable and try another one or another USB port.
Check that the controller is on and plugged in with a cable, not just paired
over Bluetooth. `lsusb` lists everything on USB; an Xbox controller shows up as
`Microsoft Corp.`.

### `info` or `update` fails with a permission error

The udev rule from [step 4](#4-allow-xboxctl-to-use-the-controller) is missing
or has not applied yet. Run the two commands again, then unplug and replug the
controller.

Still stuck? Ask in the [discussions](https://github.com/bb441db/xboxctl/discussions).

## Notes

- See the [main page]({% link index.md %}) for all commands.
- Tested on a fresh SteamOS install on unofficial hardware (not a Steam Deck
  or Steam Machine). Installing, `list`, `info` and `update --latest` all
  worked, and a controller was upgraded to the latest firmware without
  problems. The steps should be the same on a Steam Deck or Steam Machine, but
  they have not been tried there; if you try it, please share the result in the
  [discussions](https://github.com/bb441db/xboxctl/discussions).
