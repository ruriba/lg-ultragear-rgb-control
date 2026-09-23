use hidapi::HidDevice;

const HEADER: [u8; 2] = [0x53, 0x43]; // "SC"
const TERMINATOR: [u8; 2] = [0x45, 0x44]; // "ED"

/// Largest packet: 2 (header) + 149 (sync payload) + 2 (terminator) = 153 bytes -> 192 after padding.
const MAX_PACKET: usize = 192;

/// CRC-8 (poly 0x01), used only for the final byte of the static-color command
/// (0xD2) — the rest of the protocol carries its check bytes inline. With this
/// poly the CRC degenerates to a plain XOR of every input byte (proven by the
/// `crc_degenerates_to_xor` test).
pub fn calc_crc(data: &[u8]) -> u8 {
    data.iter().fold(0u8, |a, &b| a ^ b)
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RGBColor {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

/// Frame layout: HEADER + payload + TERMINATOR, padded to a multiple of 64 and
/// written as 65-byte HID reports (Report ID 0 + 64 bytes).
/// Returns false if any write failed.
pub fn send_payload(device: &HidDevice, payload: &[u8]) -> bool {
    if payload.len() + 4 > MAX_PACKET {
        return false;
    }
    let (packet, padded) = build_packet(payload);

    let mut report = [0u8; 65]; // Report ID 0 + 64 bytes
    for (i, chunk) in packet[..padded].chunks(64).enumerate() {
        report[1..1 + chunk.len()].copy_from_slice(chunk);
        if let Err(e) = device.write(&report[..1 + chunk.len()]) {
            // Abort the rest of the frame: the monitor counts the reports of
            // a sync frame (the 0xCA arm command sets the count), so skipping
            // the remaining chunks keeps the stream aligned at the next
            // frame's first report. Pushing them through would make every
            // following frame assemble from mismatched chunks.
            eprintln!("USB write error in chunk {i}: {}", e);
            return false;
        }
    }
    true
}

/// Header + payload + terminator, zero-padded to the next 64-byte report
/// boundary, together with the padded length (the bytes actually written).
fn build_packet(payload: &[u8]) -> ([u8; MAX_PACKET], usize) {
    let total = payload.len() + 4; // header + terminator
    let padded = total.div_ceil(64) * 64;

    let mut packet = [0u8; MAX_PACKET];
    packet[..2].copy_from_slice(&HEADER);
    packet[2..2 + payload.len()].copy_from_slice(payload);
    packet[2 + payload.len()..total].copy_from_slice(&TERMINATOR);
    (packet, padded)
}

// Payload builders separated from the device writes so the exact wire bytes
// are unit-testable: any change in them is a protocol change and must be
// deliberate.

fn turn_on_payload() -> [u8; 6] {
    [0xCF, 0x02, 0x02, 0x01, 0x00, 0xDE]
}

fn turn_off_payload() -> [u8; 6] {
    [0xCF, 0x02, 0x02, 0x02, 0x00, 0xDD]
}

fn set_brightness_payload(level: u8) -> [u8; 6] {
    [0xCF, 0x02, 0x02, 0x01, level, level ^ 0xDE]
}

fn set_mode_payload(mode: u8) -> [u8; 6] {
    [0xC7, 0x02, 0x02, 0x00, mode, mode ^ 0xD7]
}

fn arm_sync_payload(mode: u8) -> [u8; 6] {
    [0xCA, 0x02, 0x02, 0x03, mode, mode ^ 0xD9]
}

/// Stores a static color into one of the 4 slots: index check
/// `slot ^ 0x0403` little-endian, then a plain-XOR CRC over header + payload.
fn store_static_color_payload(slot: u8, r: u8, g: u8, b: u8) -> [u8; 8] {
    let index_check = (slot as u16) ^ 0x0403;
    let mut payload = [0u8; 8];
    payload[0] = 0xD2;
    payload[1] = (index_check & 0xFF) as u8;
    payload[2] = ((index_check >> 8) & 0xFF) as u8;
    payload[3] = slot;
    payload[4] = r;
    payload[5] = g;
    payload[6] = b;
    let mut crc_input = [0u8; 9];
    crc_input[..2].copy_from_slice(&HEADER);
    crc_input[2..9].copy_from_slice(&payload[..7]);
    payload[7] = calc_crc(&crc_input);
    payload
}

pub fn turn_on(device: &HidDevice) -> bool {
    send_payload(device, &turn_on_payload())
}

pub fn turn_off(device: &HidDevice) -> bool {
    send_payload(device, &turn_off_payload())
}

pub fn set_brightness(device: &HidDevice, level: u8) -> bool {
    send_payload(device, &set_brightness_payload(level))
}

/// Mode switch; entering sync modes 7/8 additionally sends the chunk-count
/// command that arms the device for sync frames.
pub fn set_mode(device: &HidDevice, mode: u8) -> bool {
    let ok = send_payload(device, &set_mode_payload(mode));
    if mode == 7 || mode == 8 {
        return ok && arm_sync(device, mode);
    }
    ok
}

/// The chunk-count arm command alone (no mode switch): (re-)asserts how many
/// HID reports make up one sync frame. Re-sent after the arming settle so
/// the count is established against a quiet MCU right before the first
/// frame — a count swallowed while the MCU was still switching modes leaves
/// every frame applied partially.
pub fn arm_sync(device: &HidDevice, mode: u8) -> bool {
    send_payload(device, &arm_sync_payload(mode))
}

/// Writes a static color into one of the 4 slots, then switches to that slot's
/// mode so the color becomes visible (intentional tray UX).
pub fn set_static_color(device: &HidDevice, slot: u8, r: u8, g: u8, b: u8) -> bool {
    store_static_color(device, slot, r, g, b) && set_mode(device, slot)
}

/// Stores a static color into one of the 4 slots WITHOUT switching modes —
/// used to restore the saved palette at startup.
pub fn store_static_color(device: &HidDevice, slot: u8, r: u8, g: u8, b: u8) -> bool {
    send_payload(device, &store_static_color_payload(slot, r, g, b))
}

/// Video/audio sync: 48 RGB colors. `audio` selects the 0xC2 audio marker
/// instead of 0xC1 video — the monitor is armed for one or the other by
/// set_mode. The trailing byte is the frame's XOR check (0x42 ^ XOR of the
/// color bytes). Every component must be >= 1 or the monitor driver hangs.
pub fn send_sync_colors(device: &HidDevice, colors: &[RGBColor; 48], audio: bool) -> bool {
    send_payload(device, &build_sync_payload(colors, audio))
}

/// Builds the 149-byte payload so the frame framing is unit-testable without
/// a device.
fn build_sync_payload(colors: &[RGBColor; 48], audio: bool) -> [u8; 149] {
    let mut payload = [0u8; 149];
    payload[0] = if audio { 0xC2 } else { 0xC1 };
    payload[1] = 0x02;
    payload[2] = 0x91;
    payload[3] = 0x00;
    let mut xor = 0u8;
    for (i, c) in colors.iter().enumerate() {
        let (r, g, b) = (c.r.max(1), c.g.max(1), c.b.max(1));
        payload[4 + i * 3] = r;
        payload[5 + i * 3] = g;
        payload[6 + i * 3] = b;
        xor ^= r ^ g ^ b;
    }
    payload[148] = 0x42 ^ xor;
    payload
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_degenerates_to_xor() {
        assert_eq!(calc_crc(&[]), 0);
        assert_eq!(calc_crc(&[0x53, 0x43]), 0x53 ^ 0x43);
        let bytes: Vec<u8> = (0..=255).collect();
        let folded = bytes.iter().fold(0u8, |a, &b| a ^ b);
        assert_eq!(calc_crc(&bytes), folded);
    }

    #[test]
    fn sync_frame_markers_and_padding() {
        let black = [RGBColor { r: 0, g: 0, b: 0 }; 48];
        let video = build_sync_payload(&black, false);
        let audio = build_sync_payload(&black, true);
        assert_eq!(video[0], 0xC1);
        assert_eq!(audio[0], 0xC2);
        assert_eq!(&video[1..4], &[0x02, 0x91, 0x00]);
        // Every component is clamped to >= 1 or the monitor driver hangs.
        assert!(video[4..148].iter().all(|&b| b == 1));
        // 48 LEDs of (1^1^1) XOR-fold to 0, so the check byte is 0x42.
        assert_eq!(video[148], 0x42);
    }

    #[test]
    fn sync_frame_check_byte() {
        let colors = std::array::from_fn(|i| RGBColor {
            r: (i + 1) as u8, // 1..=48: XOR 1..48 = 48 = 0x30
            g: 0,
            b: 0,
        });
        let payload = build_sync_payload(&colors, false);
        assert_eq!(payload[148], 0x42 ^ 0x30);
        assert_eq!(payload[4], 1);
        assert_eq!(payload[4 + 47 * 3], 48);
    }

    #[test]
    fn command_payloads_are_stable() {
        // The exact wire bytes of every non-frame command: any change here is
        // a protocol change and must be deliberate.
        assert_eq!(turn_on_payload(), [0xCF, 0x02, 0x02, 0x01, 0x00, 0xDE]);
        assert_eq!(turn_off_payload(), [0xCF, 0x02, 0x02, 0x02, 0x00, 0xDD]);
        assert_eq!(
            set_brightness_payload(5),
            [0xCF, 0x02, 0x02, 0x01, 0x05, 0x05 ^ 0xDE]
        );
        assert_eq!(
            set_mode_payload(3),
            [0xC7, 0x02, 0x02, 0x00, 0x03, 0x03 ^ 0xD7]
        );
        assert_eq!(
            arm_sync_payload(8),
            [0xCA, 0x02, 0x02, 0x03, 0x08, 0x08 ^ 0xD9]
        );
        // Index check = slot ^ 0x0403 little-endian; CRC = plain XOR over
        // header + first 7 payload bytes (worked out by hand for slot 2).
        assert_eq!(
            store_static_color_payload(2, 0xFF, 0x80, 0x00),
            [0xD2, 0x01, 0x04, 0x02, 0xFF, 0x80, 0x00, 0xBA]
        );
    }

    #[test]
    fn packet_framing_header_pad_terminator() {
        // A 6-byte payload: 2 + 6 + 2 = 10 meaningful bytes -> one padded
        // 64-byte report.
        let (packet, padded) = build_packet(&set_mode_payload(1));
        assert_eq!(padded, 64);
        assert_eq!(&packet[..2], b"SC");
        assert_eq!(&packet[2..8], &set_mode_payload(1));
        assert_eq!(&packet[8..10], b"ED");
        assert!(packet[10..].iter().all(|&b| b == 0));

        // A 149-byte sync payload: 153 meaningful bytes -> three reports.
        let black = [RGBColor { r: 0, g: 0, b: 0 }; 48];
        let (packet, padded) = build_packet(&build_sync_payload(&black, false));
        assert_eq!(padded, 192);
        assert_eq!(&packet[..2], b"SC");
        assert_eq!(&packet[151..153], b"ED");
        assert!(packet[153..].iter().all(|&b| b == 0));
    }
}
