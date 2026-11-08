---
permalink: /nano/
title: Xbox Nano Controller
nav_order: 5
description: "The Xbox Nano Controller (045e:0b4c, codename TALA): a HID device that speaks Microsoft's Companion Protocol instead of GIP. How xboxctl supports it, and reverse engineering notes on the protocol and the Xbox Accessories app's SDK."
---

# Xbox Nano Controller
{: .no_toc }

The Xbox Nano Controller (2026, codename **TALA**, USB ID `045e:0b4c`) is not a
GIP controller. Over USB it is a plain HID device, and the Xbox Accessories app
manages it with a different protocol that its SDK calls the *Companion
Protocol*. It runs on a Realtek AmebaSmart Wi-Fi and Bluetooth chip, and its
firmware is not in either public feed `xboxctl` otherwise uses, only in a
separate flight feed (below).

`xboxctl list`, `xboxctl info` and `xboxctl update` support it; the other
commands are GIP-specific and say so instead of failing confusingly:

```bash
xboxctl list                 # shown alongside GIP controllers
xboxctl info                 # hardware revision, firmware version, updatable?
xboxctl update --latest      # current firmware for its hardware revision, from the flight feed
xboxctl update <image.bin>   # a PPAS firmware image
```

This has been checked against an emulated Nano in QEMU over a real USB HID
connection, including a full firmware update (session, device info, and the
write/finish sequence) — **not yet against a physical Nano**; see
[Open questions](#open-questions).

The rest of this page is reverse engineering notes on the protocol itself and
on the Xbox Accessories app, from its 2610.2610.13002.0 build (October 2026),
mostly its native `xaccessories_sdk_uwp.dll`, decompiled with Ghidra, and from
Nano firmware images, checked against that app with an emulated device in QEMU
(see [Emulating a Nano](#emulating-a-nano)). All integers are little-endian.

<details open markdown="block">
  <summary>Contents</summary>
  {: .text-delta }
- TOC
{:toc}
</details>

## Identification

In the app the Nano is **Pendragon Device C**, one of a new family of
accessories (Pendragon devices B to E). Its picture is
`Assets/Images/PendragonDeviceC.png` and its name resource is
`NanoControllerName` ("XBOX Nano Controller").

| codename | USB ID | app name |
|----------|--------|----------|
| **TALA** | `045e:0b4c` (dev boards), retail `045e:0b4e`, Bluetooth LE `0b4f` | Pendragon Device C, the Nano |
| ACTI | `045e:0b1d` | Pendragon Device B |
| TORN | `045e:0b46` | Pendragon Device D |
| WATE | `045e:0b45` | Pendragon Device E |

The app and its firmware feeds only know TALA as `0b4c`; the retail `0b4e` ID
(bcdDevice `0x0609`) comes from elsewhere (see
[Who actually updates a Nano](#who-actually-updates-a-nano)). The app's
firmware metadata distinguishes hardware revisions of these devices
(`045E_0B4C-hw-0C06` and so on, matched regardless of USB product ID); retail
Nanos are presumably revision `0C06`.

## USB and HID transport

The SDK finds Pendragon devices with
`HidDevice.GetDeviceSelector(usagePage: 0xff45, usageId: 0x63)`, independent of
their USB IDs. Its `HidCompanionTransport` writes and reads only **report ID
`0x0c`**:

- **Send**: a buffer of the output report length, `0c` followed by the message,
  zero padded, written with `WriteFile`.
- **Receive**: a report is accepted only if it fills the whole input report
  length and starts with `0c`; at most 250 message bytes follow the ID. Anything
  else is discarded.

The emulated device that the app accepts has one HID interface with a vendor
collection (usage page `0xff45`, usage `0x63`), report ID `0x0c`, and 63-byte
input and output reports, and no GIP interfaces. Report sizes on the real
controller are unknown.

## Messages

Every report carries one message. The first byte selects its kind.

### Session (control)

```
ff <seq> <session id:u32> <version: 3 bytes> <max message:u16>
```

The host opens a session with a random session ID, sequence `00`, version
`02 00 16` (2.0.22) and maximum message size `0x1002`. The device answers with
the same layout, the same session ID and a different sequence byte; that byte
becomes the session **handle** used in all following messages. The version must
be at least 2. A reply with an unchanged sequence byte is treated as a
duplicate and does not open the session.

### Data and acknowledgements

```
data: <type> <handle> <opcode> <seq> <length> <payload>
ack:  02 <handle> <opcode> <seq + 1>
```

| type | meaning |
|------|---------|
| `01` | middle fragment |
| `03` | first fragment |
| `04` | last fragment |
| `05` | complete message in one fragment |

Transfers are stop-and-wait: the receiver acknowledges every fragment with the
next sequence number, and the sender waits for that before sending the next one.
A reassembled message can be up to the negotiated maximum (`0x1002`). A reply
uses the request's opcode and its own sequence numbers. Some commands are sent
without expecting a reply.

## Requests

The SDK has about twenty request opcodes (`02 03 05 07 0b 0d 0e 0f 10 17 1d
1e 1f 20 22 24 25 29 2b 2e` and a few computed ones). Those understood so far:

### `2e`: device information

The app's first request after the session opens. Request payload: a u64
timestamp and the u32 `0x3b`. The reply is 78 bytes:

| offset | field |
|--------|-------|
| `0x00` | vendor ID (u16) |
| `0x02` | product ID (u16) |
| `0x04` | hardware revision (u16), matched against the firmware metadata |
| `0x06` | 16 bytes |
| `0x16` | 14 bytes |
| `0x24` | u8 |
| `0x25` | u32 |
| `0x29` | u32 capability bits |
| `0x2d` | u32, u16 |
| `0x33` | firmware version, 4 × u16, must not be all zero |
| `0x3b` | u32, u16, u16, u64 |
| `0x4b` | variant tag (`00`: nothing follows) |
| `0x4c` | 2 bytes |

Capability bits seen so far: bit 2 makes the device updatable (the app shows
its update state) and allows power commands; bit 4 enables requests `20`/`22`;
bit 7 enables crash dump upload (opcode `0d`). Devices on older protocol
versions get requests `1c` or `23` instead of `2e`.

### `1e` and `1f`: external messages

A generic channel to a component: the request payload is a component byte and
data; `1e` expects a reply, `1f` does not. The reply is a status byte and data:
`00` success, `02` invalid argument, `43` not supported. After the device
information the app sends `1e` with payload `02 05`; its meaning is chosen by
the app, not the SDK, and is unknown.

### `24`: power

`24 01` restarts the device and `24 00` turns it off. Sent without expecting a
reply, and only to devices with capability bit 2.

### `0d`: crash dumps

When capability bit 7 is set, the app reads a crash dump in chunks, stores it
under a `Dumps` folder and uploads it to Windows Error Reporting
(`XboxAccessoriesFirmwareCrash`).

## Firmware update

Opcode `03` carries the update. A subcommand byte starts the payload, and every
reply starts with the subcommand and a status byte.

| subcommand | request | reply |
|------------|---------|-------|
| `14` status | `14` | 80 bytes (below) |
| `11` start (`15` in mode 5) | `11`, 16-byte token, new firmware version (4 × u16), a byte, mode, `00` | `11 <status> <resume offset:u32> <byte>` |
| `12` write | `12 <byte> <offset:u24> <data>` | `12 <status> [flags]`, flag bit 0: restart needed |
| `13` finish | `13 <byte> <byte>` | `13 <status>` |

The `14` reply holds a 16-byte token at offset 2 (echoed in `11`), the running
firmware version at 18 (4 × u16, must not be zero), flags at 26 (bit 0:
updating), the mode for `11` at 27, the maximum write size at 32 (u16, must not
be zero), a pending flag at 34, the pending image's version at 52, bytes written
at 60 and the transfer size at 68.

An update runs:

1. `14`: read the token, mode and maximum write size.
2. `11`: start; the reply gives the offset to resume from.
3. `12`: write the image from that offset to the end, in pieces of the smaller
   of the session maximum minus 5 and the device's maximum write size.
4. `13`: finish.

Update status bytes:

| status | meaning |
|--------|---------|
| `00` | success |
| `05` | the device is not updating |
| `06` | the device failed during the update |
| `07` | offset out of range |
| `08` | flash write error |
| `09` | the firmware image could not be used |
| `01`, `02`, `04` | other update errors |

The single bytes in `11`, `12` and `13`: `11`'s flags byte is
`foreground << 4` (`10` foreground, `00` background); its last reply byte is a
slot chosen by the device, echoed in every `12` and in `13` (`xboxctl` and the
fake device above only exercise `0`). `13`'s flags byte is `1` for a
foreground update, `0` for background; `4` means a user granted update consent
and is only valid while the device is waiting for it (status mode `5`), which
`xboxctl` does not implement.

### Firmware images

Nano firmware comes from a separate *flight feed*
(`feed::NANO_FEED_URL` in `xboxctl`'s source), in the same format as the
public feed plus a hardware revision in each entry (`-hw-XXXX`) and a
`reqApp` field. `xboxctl` matches entries by vendor ID and hardware revision,
not by USB product ID, since one hardware revision can show up under more
than one (a Nano's development and retail boards, for instance). Images are
`PPAS` files that the SDK sends unmodified:

| offset | field |
|--------|-------|
| `0x00` | `PPAS` |
| `0x04` | u32 format version (2) |
| `0x0c` | firmware version, 4 × u16 |
| `0x14` | u32 file size |
| `0x18` | u32 entry count (7) |
| `0x20` | table, starting with base address `0x08000000` |

The rest is compressed firmware for a Realtek AmebaSmart chip (FreeRTOS,
Realtek's OTA boot loader) with a few blocks that look like signatures. The
device checks the image itself.

### Who actually updates a Nano

The app itself never starts an update: it only shows the device's reported
state, and hands any update to a Windows service, `XboxAccessoriesService`,
through a COM broker (`XAUwpComBroker.exe`) in its package. That service is
not part of the app. It is installed by Windows Update as a driver extension,
`xboxaccessoriesservice.inf` (`XBOXAccessoriesService.exe`,
`xaccessories_sdk_svc.dll`, 0.2.31.0, 2026-10-05), alongside
`xboxgamecontrollerdriver.inf` for the gamepad HID collection. The extension
triggers only on the retail Nano's real USB ID, `045e:0b4e` (bcdDevice
`0x0609`, per SDL issue 16657), specifically its companion collection's
device interface arriving (`HID\VID_045E&PID_0B4E&Col04`, the fourth
top-level HID collection).
It never installs for the `0b4c` development board's ID or for the USB node
itself, so a lone emulated `0b4c` Nano never gets the service and the app
reports "Update required" without ever updating it.

With `XboxAccessoriesService` running — its trigger just needs a `0b4e`
device's companion collection to appear, even a different device from the one
being updated — it runs the same status/start/write/finish sequence as
`xboxctl` does, automatically and in the background, with no app involvement;
the app and the service only read the device's information afterwards. So
Microsoft's own updater is this same Companion Protocol sequence, driven by a
service that a `0b4e` device's arrival starts, not logic private to the
service or to Windows.

## Open questions

- Whether a physical Nano matches all of this, including its report sizes.
  This is the main thing `xboxctl` itself has not been checked against: only
  the emulated Nano, over a real USB HID connection, not just the in-process
  mock its tests use. Relatedly, whether a physical Nano accepts an update
  driven this way at all — on the emulated Nano, the same foreground sequence
  the SDK runs on its own (without `XboxAccessoriesService`) is what `xboxctl`
  uses, and both have updated it successfully.
- The meaning of external message `1e 02 05`, and the requests behind battery
  level and the other settings pages.

A capture of a real Nano connected to Windows with the Xbox Accessories app
running would answer most of these, as would running `xboxctl` itself against
a physical Nano.

## Emulating a Nano

The [QEMU fork](https://github.com/bb441db/qemu) used for `xboxctl` testing has
a `usb-xbox-gip` device; with local changes not yet in the fork, it can pose as
a Nano:

```
-device usb-xbox-gip,product-id=0x0b4c,hid=on,gip=off,hw-revision=0x0c06,hid-reply=companion
```

`hid=on,gip=off` gives a HID-only device with the vendor collection above;
`hid-reply=companion` answers the session, acknowledges fragments, and replies
to `2e`, `1e` and the update subcommands, saving a received image to
`fw-dump-dir`. The firmware version reported to the app comes from `fw-major`,
`fw-minor`, `fw-build` and `fw-revision`. If Windows has seen the device with
other descriptors before, give it a new `serial` so it does not reuse cached
driver choices.
