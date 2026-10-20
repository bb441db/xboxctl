---
title: macOS wired driver
nav_order: 3
description: "Notes on Apple's built-in wired Xbox controller driver on macOS: what the DriverKit extension and HID service plugin do, and why wired rumble does not work."
---

# macOS wired driver
{: .no_toc }

## Summary

Wired Xbox One, Series X|S and Elite Series 2 controllers get **no rumble** on
macOS through the system driver, and apps built on it (GameController, SDL,
browsers, CrossOver/Wine) lose it too. Apple's wired driver and the HID service
plugin it feeds never implement rumble for these controllers, so no rumble
packet is ever sent; the same controller rumbles over Bluetooth and over a
direct USB connection. The controller itself is fine, and updating its
firmware does not change this.

The driver was analysed and the results below were measured on macOS 15.7
(Darwin 24.6, driver version 12.6.1, arm64e). Other macOS versions may differ.

## Components

| Piece | Path | Role |
|-------|------|------|
| `XboxGamepad.dext` | `/System/Library/DriverExtensions/` | DriverKit driver. Matches USB `045e:028e` (360), `02ea` (One S), `0b00` (Elite 2) and `0b12` (Series X\|S), exposes a HID device |
| `XboxGamepadHIDServicePlugin` | `/System/Library/HIDPlugins/ServicePlugins/` | Turns HID reports into GameController events and, for some controllers, drives haptics |
| `gamecontrollerd` | `/usr/libexec/` | Serves `GCController`, including `haptics` |

Interface class `255`, subclass `71` (`0x47`, GIP) is matched for the Xbox One
family; the 360 uses subclass `93`.

## The dext

- `XboxHIDDevice` is an `IOUserHIDDevice` subclass. One `XboxUSBDevice` service
  attaches to the USB device and an `XboxSeriesXGamepad` /
  `XboxOneSGamepad` / `XboxEliteV2Gamepad` HID service attaches to the interface.
- **The HID report ID is the GIP command byte.** The HID report descriptors
  for the three controllers expose only:
  - output `0x01`, 12 bytes (13 on the wire with the ID)
  - output `0x05`, 4 bytes (the 5-byte Start packet `05 20 01 01 00`)
  - input `0x07` (guide button) and `0x20` (input state)

  There is no output report `0x09` (rumble).
- **`setReport` is a raw pass-through.** `XboxHIDDevice::setReport` ignores the
  report ID and type and calls `_outPipe->AsyncIO(buffer, length)`. It fails only
  when there is no completion action, the pipes are not ready, no send action is
  free or the device is stopping. Nothing in the dext translates or filters
  report contents.
- **The dext never sends the Start packet** (`05 20 01 01 00` does not occur
  in the binary).
- The dext itself is signed with `com.apple.developer.driverkit.builtin` and
  lives on the sealed system volume.

## The HID service plugin

Haptics are implemented in the shared base class `GCGamepadHIDServicePlugin`,
with empty defaults that each controller class overrides:

| Hook | Base class | Xbox 360 plugin | One S, Series X\|S, Elite 2 plugins |
|------|------------|-----------------|--------------------------------------|
| `dispatchHapticEvent` | `ret` (no-op) | implemented | **not overridden** |
| `defaultHapticMotors` | empty `NSArray` | implemented | **not overridden** |
| `updateHaptics:` | `ret` (no-op) | not overridden | not overridden |

The 360 plugin builds the classic 8-byte report `00 08 00 <left> <right> 00 00 00`
and sends it with `setReport:reportLength:withIdentifier:forType:error:`. The
One S, Series X|S and Elite 2 plugins each implement only
`handleInputPayload:withData:timestamp:` (Elite 2 also
`dispatchPowerOnMessageCompletion:`). So nothing in these plugins builds a GIP
rumble packet (command `0x09`).

This also explains the reported symptoms:

- **`GCController.haptics` advertises motors but nothing moves.** The
  advertised capabilities do not come from the plugin: its motor list is empty
  and its dispatch is a no-op, so `gamecontrollerd` plays patterns with no
  error.
- **Output report `0x09` returns `kIOReturnAborted`** (reproduced). `0x09` is
  not in the descriptor, and HIDDriverKit rejects it before the dext sends
  anything; see Measured on hardware.

## The Bluetooth plugin (`XboxOneHIDServicePlugin`)

A second plugin, `/System/Library/HIDPlugins/ServicePlugins/XboxOneHIDServicePlugin.plugin`,
handles Bluetooth controllers and **does** implement rumble. Findings:

- Its `matchService:options:score:` accepts vendor `045e` and a list of PIDs
  (`02ea`, `02fd`, `02e0`, `0b20`, `0b02`, `0b05`, `0b22`, `0b0c`, `0b21`,
  `0b13`, ...), but returns no match when the `Transport` property equals
  `USB`. The PID list also includes the USB PID `02ea`, so the transport check
  is what excludes wired controllers.
- Its `dispatchHapticEvent` builds a 9-byte report with ID `0x03`: a motor
  enable mask (`0x0f`), four magnitudes scaled to 0-100 (trigger left and right,
  then the two handles), a duration byte and two zero bytes. This is the
  Bluetooth HID output report, not GIP.
- The `XboxWirelessGamepad` class in the dext takes an `IOUSBHostDevice`
  provider, but no personality in the dext's `Info.plist` instantiates it and
  its entitlement only lists the four controller PIDs, so it is not reachable
  on 15.7. The matching `XboxWirelessGamepadHIDServicePlugin` has no haptics.

**Measured over Bluetooth Low Energy** (same Series X|S, firmware 5.23.6.0, macOS
15.7): rumble works in the browser Gamepad API. The controller appears as
product ID `0b13` (the plugin's list includes it), as an `IOHIDUserDevice`
with `Transport "Bluetooth Low Energy"` and `MaxOutputReportSize 9`, which is
the nine-byte report `0x03` the plugin builds. The descriptor has an output
report with ID `3`. Apple's wired dext is not involved, so rumble goes through
the plugin's `dispatchHapticEvent`. `gamecontrollerd` still lists the same
synthetic Xbox 360 pad as in the wired case.

Forcing the Bluetooth plugin onto a wired controller would not work even if the
`Transport` check could be spoofed: the dext forwards report bytes untouched
to the USB OUT endpoint, where byte 0 is the GIP command. A `03 0f ...` packet
is GIP command `0x03`, not rumble (`0x09`).

## Measured on hardware

Series X|S, firmware 5.23 (`bcdDevice 0x0517`), wired, macOS 15.7.

**Registry.** The USB device's `UsbExclusiveOwner` is `XboxUSBDevice`. The dext
publishes one HID device, `XboxSeriesXGamepad` (`AppleUserHIDDevice`), with
`Transport "USB"`, `MaxOutputReportSize 13`, `MaxInputReportSize 19`,
`ReportInterval 4000` and the descriptor extracted from the binary. `gamecontrollerd`
adds an `AppleGCSyntheticDevice` ("GamePad-1") that looks like an Xbox 360 pad
(product ID `028e`) with `MaxOutputReportSize 0`. That synthetic device is what
apps see, and it has no output at all.

**Output reports through `IOHIDDeviceSetReport`.** For ID `0x09` and `0x00` the
buffer was the 13-byte GIP rumble packet, for the others it was zeros after the
report ID:

| report ID | length | in descriptor | result |
|-----------|--------|---------------|--------|
| `0x05` | 5 | yes | success |
| `0x01` | 13 | yes | success |
| `0x01` | 5 | wrong length | `kIOReturnAborted` |
| `0x00`, `0x02`, `0x03`, `0x09` | 13 | no | `kIOReturnAborted` |

The dext's own log shows where the rejection happens: `IOHIDDevice.cpp
ProcessReport … ret:0xe00002c2` (`kIOReturnBadArgument`) for each rejected
report, and no `AsyncIO` line. HIDDriverKit inside the dext checks the report
ID and length against the descriptor before `XboxHIDDevice::setReport` runs,
and the user-level result is `kIOReturnAborted`. Reports that pass are sent to
the controller unchanged, with the report ID as the first byte on the wire, so
only commands `0x01` and `0x05` can ever be sent through the HID device.
Rumble (`0x09`) cannot.

## Checking the controller's rumble

`xboxctl rumble` talks to the controller directly over USB and shows whether the
motors work regardless of the system driver; see
[Testing rumble]({% link index.md %}#testing-rumble).

## Reports

- Wired rumble not working on macOS (27.0.1) was mentioned in
  [issue #8](https://github.com/bb441db/xboxctl/issues/8).
- It has been reported to Apple as FB25254769. Filing a duplicate that mentions
  that number helps Apple prioritise it.
