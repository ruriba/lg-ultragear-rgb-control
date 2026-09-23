# Monitor HID protocol — reverse-engineering notes

Everything here was verified against 27GN950/38GN950/38GL950G and cross-checked
with the Python reference implementation from
[Bairminer/LG-Ultragear-RGB-Control](https://github.com/Bairminer/LG-Ultragear-RGB-Control).
The live code is `src/usb_protocol.rs` (framing) and `src/usb.rs` (device
acceptance, worker).

## Transport and device

- Device: VID `0x043E`, PID `0x9A8A` (requires usage page `0xFF01` or `0`) or
  PID `0x9A57`.
- Every write is a **65-byte** HID report: Report ID `0x00` + 64 bytes.
- Frame: `"SC"` header (`53 43`) + payload + `"ED"` terminator (`45 44`),
  zero-padded to a multiple of 64. A full sync frame is 149 bytes of payload
  → 192 padded → **3 reports per frame**.

## Commands

| Command | Payload (after the header) |
| --- | --- |
| Turn on | `CF 02 02 01 00 DE` |
| Turn off | `CF 02 02 02 00 DD` |
| Brightness `n` | `CF 02 02 01 n n^0xDE` |
| Mode `m` | `C7 02 02 00 m m^0xD7` |
| Sync mode `m` (7/8) | the above + `CA 02 02 03 m m^0xD9` (arms the mode) |
| Static color, slot `s` | `D2 s^03 04 s r g b crc8` (index check = `s ^ 0x0403` as two little-endian bytes; crc8 = XOR of header+payload, see below) |

Modes: 1-4 = static color slots, 5 = Peaceful, 6 = Dynamic, **7 = Audio
Sync**, **8 = Video Sync**. Check bytes travel inline inside each payload; the
exception is the static-color command, whose last byte is a CRC-8 (poly
`0x01`) that degenerates into a plain XOR of all preceding bytes.

## Sync frames (video `0xC1` / audio `0xC2`)

149-byte payload:

```text
C1|C2 02 91 00 | 48 × (R G B) | check
```

- Every color component **must be ≥ 1**: a zero hangs the monitor driver (the
  code forces `max(1)`).
- `check = 0x42 ^ XOR(R1..B48)`.

## Firmware behavior observed on hardware

- **Sync timeout ≈ 12 s**: if the monitor stops receiving sync frames for that
  long it leaves mode 7/8 by itself and reverts to its last static preset,
  with no HID notification. This is why the engine sends a keepalive frame
  every 5 s while the scene is static (see `SYNC_KEEPALIVE` in
  `src/engine.rs`).
- **Brightness commands land while armed**: the device accepts brightness
  changes even while it stays armed in a sync mode (verified: setting
  brightness during image sync and then switching to a static mode keeps the
  new level). Power commands, by contrast, are ignored until the sync timeout
  or a mode change disarms it.
- DXGI duplication can deliver BGRA8 frames even when the duplication was
  created as FP16/HDR; the delivered frame's format is authoritative and is
  re-checked every frame.
- There is no state read-back over HID: whatever the app believes the monitor
  shows is a mirror of what it has sent.
