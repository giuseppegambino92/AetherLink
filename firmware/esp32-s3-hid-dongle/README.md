# AetherLink ESP32-S3 HID dongle

Turns an ESP32-S3 board into a keyboard + mouse that AetherLink drives over
USB-serial, so any machine running AetherLink can control a device that only
accepts real input hardware: iPhone/iPad (BLE, pointer through AssistiveTouch),
locked PCs, BIOS screens, TVs.

```
AetherLink ──USB CDC or UART──► ESP32-S3 ──BLE HID (NimBLE)──► iPhone / iPad / any BLE host
                                       └──USB HID (native port)──► USB host
```

Both links use AetherLink's report descriptor (keyboard ID 1, relative mouse
ID 2, absolute pointer ID 3, consumer ID 4) — byte-identical to
`crates/aether-input/src/hid_report.rs` (a Rust unit test checks the sketch).

## Hardware

Any ESP32-S3 board with native USB (ESP32-S3-DevKitC-1, XIAO ESP32S3, …).

* **BLE target** (iPhone/iPad): plug the S3's native USB port ("USB") into the
  AetherLink computer. It shows up as `/dev/cu.usbmodem…` / `/dev/ttyACM…` /
  `COMn` with USB VID `303A`.
* **USB target**: the native port goes to the *target*, so AetherLink must talk
  to the dongle through UART0 — on DevKitC the second connector ("UART",
  CP210x/CH34x bridge, `/dev/cu.usbserial…` / `/dev/ttyUSB…`) at 921600 baud.
  AetherLink lists those bridges too and confirms the dongle with a Hello.

Note: the native port always enumerates as CDC **and** HID, so the AetherLink
computer also sees an (idle) keyboard; macOS may open "Keyboard Setup
Assistant" once — just close it.

## Build & flash (arduino-cli)

```sh
arduino-cli config init --additional-urls https://espressif.github.io/arduino-esp32/package_esp32_index.json
arduino-cli core update-index
arduino-cli core install esp32:esp32          # tested with 3.3.12
arduino-cli lib install NimBLE-Arduino        # tested with 2.5.2

FQBN=esp32:esp32:esp32s3:USBMode=default,CDCOnBoot=cdc   # USB-OTG (TinyUSB) + CDC on boot
arduino-cli compile --fqbn $FQBN firmware/esp32-s3-hid-dongle
# First flash: hold BOOT, tap RESET, release BOOT (ROM download mode), then:
arduino-cli upload  --fqbn $FQBN -p /dev/cu.usbmodem1101 firmware/esp32-s3-hid-dongle
```

`USBMode=default` (TinyUSB) is required: the HID device and the CDC port share
the native USB controller. With TinyUSB firmware running, later uploads work
without the BOOT button (the core's 1200-baud touch resets into the bootloader).

Compile status: builds cleanly with esp32 core 3.3.12 + NimBLE-Arduino 2.5.2
(`arduino-cli compile`, Linux arm64, ~633 KB flash / 66 KB RAM). Not yet flashed
or exercised on hardware.

## Pairing an iPhone / iPad

1. In AetherLink choose the dongle backend, target BLE, then "Pair" — this
   sends `Pairing/StartAdvertising` (the dongle is **not** discoverable until
   asked; with an existing bond it only advertises so the bonded host can
   reconnect).
2. iPhone: Settings → Bluetooth → "AetherLink HID" (Just Works pairing).
3. For the pointer: Settings → Accessibility → Touch → AssistiveTouch → on.

`Pairing/ClearBonds` forgets all bonded hosts.

## Wire protocol (v1)

```
A5 5A | ver=01 | type | seq | len | payload[len ≤ 64] | crc16 (LE)
```

CRC-16/CCITT-FALSE (poly 0x1021, init 0xFFFF) over `ver..payload`. Every
host → dongle frame gets `Ack [seq, status]` (0 ok, 1 unknown type, 2 bad
length, 3 no target link connected, 4 bad argument, 5 version mismatch).

| type | dir | payload |
|---|---|---|
| 0x01 Hello | → | `[protocol_version]`; answered by Ack + Info |
| 0x02 Info | ← | `[proto, fw_major, fw_minor, fw_patch, caps, target, max_payload, 0]` |
| 0x10 Keyboard | → | report 1 payload (8 B) |
| 0x11 Mouse | → | report 2 payload (7 B) — never retransmitted |
| 0x12 AbsPointer | → | report 3 payload (5 B) |
| 0x13 Consumer | → | report 4 payload (2 B) |
| 0x20 SetTarget | → | `[1=USB, 2=BLE, 3=both]` |
| 0x21 Pairing | → | `[op, arg]`: 0 stop adv, 1 start adv (arg = seconds, 0 = forever), 2 clear bonds, 3 disconnect |
| 0x30 GetStatus | → | empty; answered by Ack + Status |
| 0x31 Status | ← | `[target, usb_mounted, ble_connected, ble_advertising, leds, bond_count]`, also sent on change |
| 0x32 Leds | ← | `[led bitmap]` (reserved; LEDs are reported in Status) |
| 0x7E Ack | ← | `[acked_seq, status]` |

Example: `Hello` = `A5 5A 01 01 00 01 01` + CRC of `01 01 00 01 01`.

The reference implementation (encoder, resyncing decoder, ack/retransmit
tracker, tests) is `crates/aether-input/src/bt_hid/frame.rs`.
