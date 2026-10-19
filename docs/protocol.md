---
title: Firmware update protocol
nav_order: 2
description: "Reverse engineered documentation of the Xbox controller firmware update protocol (UFU over GIP): framing, messages, update sequence, restarts, flash layout and the rdhm package format."
---

# Xbox controller firmware update protocol (UFU over GIP)
{: .no_toc }

Microsoft's controllers are updated with a protocol that Windows calls UFU,
carried in GIP messages over the controller's interrupt endpoints (interface 0,
OUT `0x02`, IN `0x82`). It was reverse engineered from the Windows driver and
`Windows.Gaming.Input.dll`, then verified against the QEMU emulator and a
physical Xbox Series X|S controller. All integers are little-endian.

This is the protocol [`xboxctl`]({{ '/' | relative_url }}) implements; the source is in
[`src/gip/`](https://github.com/bb441db/xboxctl/tree/main/src/gip).

<details open markdown="block">
  <summary>Contents</summary>
  {: .text-delta }
- TOC
{:toc}
</details>

## GIP framing

Every packet starts with `command, flags, sequence, length`, where the length
is a base-128 varint. Relevant flags:

| flag | meaning |
|------|---------|
| `0x10` | acknowledgement requested |
| `0x20` | system message (all UFU traffic) |
| `0x40` | first fragment |
| `0x80` | fragment |

Sequence numbers run from 1 to 255; 0 is skipped.

- **Acknowledgement** (command `0x01`): `01 20 <seq> 09 | 00 <command> 20 <received:u16> 00 00 <remaining:u16>`.
  The controller requests one for each of its UFU replies (`0c 30 …`), and the
  host has to send it.
- **Fragmentation**: messages that don't fit one 64-byte packet are split into
  58-byte fragments with 6-byte headers. Host-to-device headers must have an
  even length, so one varint is padded to two bytes. The first fragment carries
  the total length, later ones their offset. Acknowledgements are requested on
  the first and last fragment and periodically in between (`xboxctl`: every
  fifth); the receiver reports the contiguous bytes received, and the sender
  resends from there. A
  zero-length fragment at offset = total ends the message.
- **Set Device State** (command `0x05`, one-byte payload): `00` start, `07` reset.
- **Status** (command `0x03`): a first byte with bits 6–7 clear means powered off.

## UFU messages

UFU requests and replies are the payload of GIP command `0x0c` with the system
flag. Replies echo the opcode, followed by a status byte.

| op | request | reply |
|----|---------|-------|
| `00` GetInfo | `00` | 60 bytes: `00 st protocol flags vid:u16 pid:u16 max_write:u32 …` |
| `01` GetVersion | `01 component:u32` | `01 st component:u32 version:u16×4`, optionally `magic 0x1A253A45, partition_address:u32, partition_size:u32` |
| `02` BeginUpdate | `02 component:u32 address:u32 size:u32 version:u16×4` | `02 st` |
| `03` GetBlockHash | `03 address:u32 length:u32` | `03 st sha1[20]` of the flash in that range |
| `04` WriteBlock | `04 00 00 00 address:u32 data` (fragmented) | `04 st` |
| `05` CompleteUpdate | `05` | `05 st` |

Status codes: 0 ok, 1 invalid argument, 2 unknown component, **3 must reset**,
4 component in use, 5 invalid range, 6 write failure, 7 write protected, 8 update
in progress, 9 update not in progress, 10–12 EEPROM errors, 13 protocol error,
14 incompatible version.

Protocol 1 is the original Xbox One controller's legacy variant and is not
supported. The Series X|S controller reports protocol 2 and a 4 KiB write size.

## Update sequence

1. Start the controller (Set Device State `00`).
2. **GetInfo**: the VID/PID must match the package. Blocks are
   `min(max_write, 0x1000)` rounded down to 4 bytes.
3. **GetVersion** for every component in the package. Components whose
   flags have bit 0 set (critical, e.g. the bootloader) are never written.
   A component larger than the reported partition does not fit this controller.
4. **Order**: if component 3 fits its partition (`size < partition_size`),
   update in the order 4, 7, 3, 2, 1, otherwise 2, 1, 4, 7, 3; ids not in the
   package are skipped. The partition size comes from GetVersion if reported.
   A Series X|S controller reports a 0x7e000 partition for component 3, so 5.23
   packages are written in the order 2, 4, 3.
5. For each component whose version differs: **BeginUpdate**, then for every
   block **GetBlockHash** and, if the SHA-1 differs, **WriteBlock**, then
   **CompleteUpdate**.
6. Repeat the version check (up to 10 passes) until every component matches,
   or until a pass writes nothing.
7. **Reset** (Set Device State `07`). The controller restarts into the new
   firmware.

The Windows updater waits 4 s for each reply (6 s for WriteBlock) and sends
each request up to 5 times.

## Restarts

When a reply has status 3 (must reset), the host resets the controller and
continues once it is back. The Series X|S controller asks for this after every
CompleteUpdate and once before writing component 4, so a full update restarts
it five times, including the final reset.

A restart looks like this on the wire:

1. The host sends Set Device State `07`.
2. The controller replies with a powered-off status (`03 20 ss 04 00 …`).
3. 1–3.6 s later it drops off USB, then re-enumerates at a new address.
4. It sends **one** Hello, which the host must answer promptly with Set Device
   State `00` and a non-zero sequence number. Windows answers within 15–31 ms.
   A controller whose Hello is not answered stops responding until it is
   unplugged. The Windows updater gives the controller 30 s to come back
   (60 s after component 7, which only the Adaptive Controller has).
5. The update continues with GetVersion; a component completed before the
   restart now reports its new version.

## Series X|S controller (`045e:0b12`, MERL) layout

| id | flash address | partition | contents |
|----|---------------|-----------|----------|
| 1 | `0x10000000` | `0x5000` | bootloader; never updated (5.9.2709.0 on the tested controller) |
| 2 | `0x10005000` | `0x22000` | encrypted or compressed image |
| 3 | `0x10027000` | `0x7e000` | application (Cortex-M); its version is the release version |
| 4 | `0x10027000` | `0x7e000` | second application image, purpose unknown; own version series (e.g. 5.23.330.0) |

The signed components (1, 3, 4) end in a 0x180-byte block, probably an RSA-3072
signature that the controller checks; the host performs no signature checks.

## rdhm packages

Firmware files (`MERL_045E_0B12_v_5_23_6_0_h_<sha256>.bin`) are `rdhm`
containers. The `<sha256>` in the name is the base64url SHA-256 of the file.

| offset | field |
|--------|-------|
| `0x00` | magic `rdhm` |
| `0x04` | header version (1) |
| `0x08` | VID:u16, PID:u16 |
| `0x0c` | package version u16×4 |
| `0x14` | minimum version u16×4 |
| `0x1c` | number of components (≤ 10) |
| `0x40` | component entries, 0x40 bytes each |

Each entry holds the component id, flags (bit 0 critical), file offset, size,
target address, an unused word, version u16×4 and minimum version u16×4. The
file is exactly the header, the entries and the component data; the host sends
only the component data, never the header.

## Firmware feed

`https://aka.ms/xboxaccessoriesfwcdnpcga` redirects to the JSON index the Xbox
Accessories app uses: `g` feed name, `v` version, `p` download base URL, and
`d` devices. Each device has `__n` codename, `id` (`VID_PID`, sometimes with a
`-cc-` suffix), `f` firmware files (`p` file name) and `req` (minimum version).
A firmware file is downloaded from `p` + its file name.
